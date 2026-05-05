use std::time::Duration;

use anyhow::Context;
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{Namespace, PersistentVolumeClaim, Pod, Secret, Service, ServiceAccount},
    rbac::v1::{ClusterRole, ClusterRoleBinding},
};
use kube::{
    api::{DeleteParams, ListParams, Patch, PatchParams, PostParams},
    Api, Client,
};
use serde_json::json;
use tracing::{error, info};

pub const NAMESPACE:         &str = "octo";
pub const LAIR_NOISE_PORT: u16  = 30900;

const NODEPORT_MIN:  u16  = 30100;
const NODEPORT_MAX:  u16  = 30199;
const IMAGE:         &str = "ghcr.io/georgebradford0/lair:latest";
const ENTRYPOINT:    &str = "/usr/local/bin/docker-entrypoint-server.sh";
const LAIR_NAME:   &str = "lair";

// ── Existing child-management types and functions ─────────────────────────────

pub struct ChildInfo {
    pub name:       String,
    pub git_url:    String,
    pub status:     String,
    pub noise_port: u16,
}

pub async fn build_client() -> anyhow::Result<Client> {
    Ok(Client::try_default().await?)
}

pub async fn list_managed_deployments(client: &Client) -> anyhow::Result<Vec<ChildInfo>> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    let list = deployments
        .list(&ListParams::default().labels("octo.managed=1"))
        .await
        .context("list deployments")?;

    let services: Api<Service> = Api::namespaced(client.clone(), NAMESPACE);
    let mut results = Vec::new();

    for d in list {
        let name        = d.metadata.name.unwrap_or_default();
        let annotations = d.metadata.annotations.unwrap_or_default();
        let git_url     = annotations.get("octo.git_url").cloned().unwrap_or_default();

        let status = {
            let st        = d.status.as_ref();
            let available = st.and_then(|s| s.available_replicas).unwrap_or(0);
            let replicas  = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0);
            if available > 0        { "running" }
            else if replicas == 0   { "stopped" }
            else                    { "pending" }
        }.to_string();

        let noise_port = match services.get(&format!("{name}-noise")).await {
            Ok(svc) => svc.spec.as_ref()
                .and_then(|s| s.ports.as_ref())
                .and_then(|ps| ps.first())
                .and_then(|p| p.node_port)
                .map(|p| p as u16)
                .unwrap_or(NODEPORT_MIN),
            Err(_) => NODEPORT_MIN,
        };

        results.push(ChildInfo { name, git_url, status, noise_port });
    }

    Ok(results)
}

pub async fn assign_nodeport(client: &Client) -> anyhow::Result<u16> {
    let services: Api<Service> = Api::namespaced(client.clone(), NAMESPACE);
    let list = services.list(&ListParams::default()).await.context("list services")?;

    let used: std::collections::HashSet<u16> = list.iter()
        .filter(|s| s.metadata.name.as_deref().map(|n| n.ends_with("-noise")).unwrap_or(false))
        .flat_map(|s| {
            s.spec.as_ref()
                .and_then(|spec| spec.ports.as_ref())
                .into_iter()
                .flatten()
                .filter_map(|p| p.node_port.map(|n| n as u16))
        })
        .collect();

    (NODEPORT_MIN..=NODEPORT_MAX)
        .find(|p| !used.contains(p))
        .ok_or_else(|| anyhow::anyhow!("no free NodePorts in {NODEPORT_MIN}–{NODEPORT_MAX}"))
}

pub async fn read_join_token(client: &Client) -> anyhow::Result<String> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), NAMESPACE);
    let secret = secrets.get("k3s-join-token").await.context("get k3s-join-token secret")?;
    let token = secret.data
        .and_then(|d| d.get("token").cloned())
        .map(|b| String::from_utf8(b.0))
        .transpose()
        .context("decode join token")?
        .ok_or_else(|| anyhow::anyhow!("k3s-join-token secret missing 'token' key"))?;
    Ok(token)
}

pub struct CreateChildParams<'a> {
    pub name:              &'a str,
    pub git_url:           Option<&'a str>,
    pub noise_port:        u16,
    pub api_key:           &'a str,
    pub gh_token:          Option<&'a str>,
    pub pub_host:          &'a str,
    pub lair_url:        &'a str,
    pub startup_script:    Option<&'a str>,
    pub startup_prompt:    Option<&'a str>,
    /// Hex-encoded 64-byte keypair (32 private + 32 public) to inject into the child.
    pub noise_private_key: &'a str,
}

pub async fn create_child_resources(client: &Client, p: &CreateChildParams<'_>) -> anyhow::Result<()> {
    create_pvcs(client, p.name).await?;
    create_deployment(client, p).await?;
    create_services(client, p.name, p.noise_port).await?;
    Ok(())
}

async fn create_pvcs(client: &Client, name: &str) -> anyhow::Result<()> {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), NAMESPACE);
    for suffix in ["data", "workspace"] {
        let pvc_name = format!("{name}-{suffix}");
        if pvcs.get(&pvc_name).await.is_ok() {
            info!("[k8s] PVC {pvc_name} already exists");
            continue;
        }
        let pvc: PersistentVolumeClaim = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "PersistentVolumeClaim",
            "metadata": { "name": pvc_name, "namespace": NAMESPACE },
            "spec": {
                "accessModes": ["ReadWriteOnce"],
                "resources": { "requests": { "storage": "10Gi" } }
            }
        }))?;
        pvcs.create(&PostParams::default(), &pvc)
            .await
            .with_context(|| format!("create PVC {pvc_name}"))?;
        info!("[k8s] created PVC {pvc_name}");
    }
    Ok(())
}

async fn create_deployment(client: &Client, p: &CreateChildParams<'_>) -> anyhow::Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);

    let meta_labels = json!({
        "octo.managed": "1",
        "app": p.name,
    });

    let mut meta_annotations = json!({});
    if let Some(url) = p.git_url {
        meta_annotations["octo.git_url"] = json!(url);
    }

    let mut env = vec![
        json!({"name": "ANTHROPIC_API_KEY",  "value": p.api_key}),
        json!({"name": "NOISE_PORT",         "value": "9000"}),
        json!({"name": "PUBLIC_PORT",        "value": p.noise_port.to_string()}),
        json!({"name": "PUBLIC_HOST",        "value": p.pub_host}),
        json!({"name": "LAIR_URL",         "value": p.lair_url}),
        json!({"name": "NOISE_PRIVATE_KEY",  "value": p.noise_private_key}),
    ];
    if let Some(url) = p.git_url {
        env.push(json!({"name": "GIT_URL", "value": url}));
    }
    if let Some(gh) = p.gh_token {
        env.push(json!({"name": "GH_TOKEN", "value": gh}));
    }
    if let Some(s) = p.startup_script {
        env.push(json!({"name": "STARTUP_SCRIPT", "value": s}));
    }
    if let Some(s) = p.startup_prompt {
        env.push(json!({"name": "STARTUP_PROMPT", "value": s}));
    }
    if std::env::var("OCTO_DEV").as_deref() == Ok("1") {
        env.push(json!({"name": "OCTO_DEV", "value": "1"}));
    }

    let pull_policy = if std::env::var("OCTO_DEV").as_deref() == Ok("1") {
        "IfNotPresent"
    } else {
        "Always"
    };

    let data_pvc      = format!("{}-data",      p.name);
    let workspace_pvc = format!("{}-workspace", p.name);

    let pod_spec = json!({
        "containers": [{
            "name": "octo",
            "image": IMAGE,
            "imagePullPolicy": pull_policy,
            "command": [ENTRYPOINT],
            "env": env,
            "ports": [
                {"containerPort": 8000, "name": "http"},
                {"containerPort": 9000, "name": "noise"}
            ],
            "volumeMounts": [
                {"name": "data",      "mountPath": "/data"},
                {"name": "workspace", "mountPath": "/workspace"}
            ]
        }],
        "volumes": [
            {"name": "data",      "persistentVolumeClaim": {"claimName": data_pvc}},
            {"name": "workspace", "persistentVolumeClaim": {"claimName": workspace_pvc}}
        ]
    });
    let deployment: Deployment = serde_json::from_value(json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": p.name,
            "namespace": NAMESPACE,
            "labels": meta_labels,
            "annotations": meta_annotations
        },
        "spec": {
            "replicas": 1,
            "selector": { "matchLabels": { "app": p.name } },
            "template": {
                "metadata": { "labels": meta_labels, "annotations": meta_annotations },
                "spec": pod_spec
            }
        }
    }))?;

    deployments.create(&PostParams::default(), &deployment)
        .await
        .with_context(|| format!("create Deployment {}", p.name))?;
    info!("[k8s] created Deployment {}", p.name);
    Ok(())
}

async fn create_services(client: &Client, name: &str, noise_port: u16) -> anyhow::Result<()> {
    let services: Api<Service> = Api::namespaced(client.clone(), NAMESPACE);

    let clusterip: Service = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": name, "namespace": NAMESPACE },
        "spec": {
            "selector": { "app": name },
            "ports": [{"port": 8000, "targetPort": 8000, "name": "http"}]
        }
    }))?;
    services.create(&PostParams::default(), &clusterip)
        .await
        .with_context(|| format!("create ClusterIP Service {name}"))?;
    info!("[k8s] created Service {name} (ClusterIP:8000)");

    let noise_svc_name = format!("{name}-noise");
    let nodeport: Service = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": noise_svc_name, "namespace": NAMESPACE },
        "spec": {
            "type": "NodePort",
            "selector": { "app": name },
            "ports": [{
                "port": 9000,
                "targetPort": 9000,
                "nodePort": noise_port as i64,
                "name": "noise"
            }]
        }
    }))?;
    services.create(&PostParams::default(), &nodeport)
        .await
        .with_context(|| format!("create NodePort Service {name}-noise"))?;
    info!("[k8s] created Service {name}-noise (NodePort:{noise_port})");

    Ok(())
}

pub async fn restart_deployments(client: &Client, names: &[&str]) -> anyhow::Result<Vec<String>> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    let now = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let s = secs % 60;
        let m = (secs / 60) % 60;
        let h = (secs / 3600) % 24;
        let mut remaining_days = secs / 86400;
        let mut year = 1970u32;
        loop {
            let days_in_year: u64 = if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) { 366 } else { 365 };
            if remaining_days < days_in_year { break; }
            remaining_days -= days_in_year;
            year += 1;
        }
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let month_days: [u64; 12] = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut month = 1u32;
        for &md in &month_days {
            if remaining_days < md { break; }
            remaining_days -= md;
            month += 1;
        }
        let day = remaining_days + 1;
        format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
    };
    let patch = json!({
        "spec": { "template": { "metadata": { "annotations": {
            "kubectl.kubernetes.io/restartedAt": now
        }}}}
    });

    let targets: Vec<String> = if names.is_empty() {
        let list = deployments
            .list(&ListParams::default().labels("octo.managed=1"))
            .await
            .context("list managed deployments")?;
        let mut t: Vec<String> = list.iter().filter_map(|d| d.metadata.name.clone()).collect();
        t.push("lair".to_string());
        t
    } else {
        names.iter().map(|s| s.to_string()).collect()
    };

    let mut restarted = Vec::new();
    for name in &targets {
        match deployments.patch(name, &PatchParams::default(), &Patch::Merge(patch.clone())).await {
            Ok(_)  => { info!("[k8s] restarted Deployment {name}"); restarted.push(name.clone()); }
            Err(e) => { error!("[k8s] restart Deployment {name} failed: {e}"); }
        }
    }
    Ok(restarted)
}

/// Patch the image to `IMAGE` and bump `restartedAt` on every managed deployment
/// plus lair itself.  Since `imagePullPolicy: Always` is set on all pods, this
/// forces each one to pull the latest image on the next start.
/// Returns the names of deployments that were successfully patched.
pub async fn update_and_restart_all(client: &Client) -> anyhow::Result<Vec<String>> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);

    let now = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let s = secs % 60;
        let m = (secs / 60) % 60;
        let h = (secs / 3600) % 24;
        let mut remaining_days = secs / 86400;
        let mut year = 1970u32;
        loop {
            let days_in_year: u64 = if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) { 366 } else { 365 };
            if remaining_days < days_in_year { break; }
            remaining_days -= days_in_year;
            year += 1;
        }
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let month_days: [u64; 12] = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut month = 1u32;
        for &md in &month_days {
            if remaining_days < md { break; }
            remaining_days -= md;
            month += 1;
        }
        let day = remaining_days + 1;
        format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
    };

    // Collect: all managed children + lair.
    let mut targets: Vec<(String, &str)> = deployments
        .list(&ListParams::default().labels("octo.managed=1"))
        .await
        .context("list managed deployments")?
        .iter()
        .filter_map(|d| d.metadata.name.clone())
        .map(|n| (n, "octo"))
        .collect();
    targets.push((LAIR_NAME.to_string(), LAIR_NAME));

    let mut updated = Vec::new();
    for (name, container_name) in &targets {
        let patch = json!({
            "spec": { "template": {
                "metadata": { "annotations": { "kubectl.kubernetes.io/restartedAt": now } },
                "spec": { "containers": [{ "name": container_name, "image": IMAGE }] }
            }}
        });
        match deployments.patch(name, &PatchParams::default(), &Patch::Strategic(patch)).await {
            Ok(_)  => { info!("[k8s] updated+restarted Deployment {name}"); updated.push(name.clone()); }
            Err(e) => { error!("[k8s] update+restart Deployment {name} failed: {e}"); }
        }
    }
    Ok(updated)
}

pub async fn scale_deployment(client: &Client, name: &str, replicas: i32) -> anyhow::Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    deployments.patch(
        name,
        &PatchParams::default(),
        &Patch::Merge(json!({"spec": {"replicas": replicas}})),
    ).await.with_context(|| format!("scale Deployment {name} to {replicas}"))?;
    info!("[k8s] scaled Deployment {name} to {replicas} replica(s)");
    Ok(())
}

pub async fn delete_child_resources(client: &Client, name: &str) -> anyhow::Result<()> {
    if let Err(e) = scale_deployment(client, name, 0).await {
        error!("[k8s] scale-to-0 {name}: {e}");
    }

    let dp = DeleteParams::default();

    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    deployments.delete(name, &dp).await.ok();
    info!("[k8s] deleted Deployment {name}");

    let services: Api<Service> = Api::namespaced(client.clone(), NAMESPACE);
    services.delete(name, &dp).await.ok();
    services.delete(&format!("{name}-noise"), &dp).await.ok();
    info!("[k8s] deleted Services for {name}");

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), NAMESPACE);
    pvcs.delete(&format!("{name}-data"),      &dp).await.ok();
    pvcs.delete(&format!("{name}-workspace"), &dp).await.ok();
    info!("[k8s] deleted PVCs for {name}");

    Ok(())
}

// ── Init (octo init) ─────────────────────────────────────────────────────

pub async fn ensure_namespace(client: &Client) -> anyhow::Result<()> {
    let api: Api<Namespace> = Api::all(client.clone());
    let ns: Namespace = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {"name": NAMESPACE}
    }))?;
    api.patch(NAMESPACE, &PatchParams::apply("octo").force(), &Patch::Apply(ns))
        .await.context("ensure namespace")?;
    Ok(())
}

pub async fn ensure_rbac(client: &Client) -> anyhow::Result<()> {
    let sa_api: Api<ServiceAccount> = Api::namespaced(client.clone(), NAMESPACE);
    let sa: ServiceAccount = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ServiceAccount",
        "metadata": {"name": LAIR_NAME, "namespace": NAMESPACE}
    }))?;
    sa_api.patch(LAIR_NAME, &PatchParams::apply("octo").force(), &Patch::Apply(sa))
        .await.context("ensure ServiceAccount")?;

    let cr_api: Api<ClusterRole> = Api::all(client.clone());
    let cr: ClusterRole = serde_json::from_value(json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {"name": LAIR_NAME},
        "rules": [
            {"apiGroups": ["apps"], "resources": ["deployments"],
             "verbs": ["get","list","watch","create","patch","delete"]},
            {"apiGroups": [""], "resources": ["services","persistentvolumeclaims","secrets",
                                              "pods","pods/exec","pods/log"],
             "verbs": ["get","list","watch","create","patch","delete"]},
            {"apiGroups": [""], "resources": ["nodes"],
             "verbs": ["get","list","watch","patch","delete"]}
        ]
    }))?;
    cr_api.patch(LAIR_NAME, &PatchParams::apply("octo").force(), &Patch::Apply(cr))
        .await.context("ensure ClusterRole")?;

    let crb_api: Api<ClusterRoleBinding> = Api::all(client.clone());
    let crb: ClusterRoleBinding = serde_json::from_value(json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRoleBinding",
        "metadata": {"name": LAIR_NAME},
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "ClusterRole",
            "name": LAIR_NAME
        },
        "subjects": [{"kind": "ServiceAccount", "name": LAIR_NAME, "namespace": NAMESPACE}]
    }))?;
    crb_api.patch(LAIR_NAME, &PatchParams::apply("octo").force(), &Patch::Apply(crb))
        .await.context("ensure ClusterRoleBinding")?;

    Ok(())
}

/// Create or update the lair-secrets Secret.
/// `noise_private_key_hex` is the hex-encoded 64-byte (private ++ public) keypair.
/// `mcp_config_json`, if provided, is stored as `MCP_CONFIG_JSON` and written to
/// `/data/mcp.json` by lair on first startup (skipped if the file already exists).
pub async fn upsert_secret(
    client: &Client,
    api_key: &str,
    gh_token: Option<&str>,
    noise_private_key_hex: &str,
    mcp_config_json: Option<&str>,
) -> anyhow::Result<()> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), NAMESPACE);
    let mut string_data = serde_json::json!({
        "ANTHROPIC_API_KEY":  api_key,
        "NOISE_PRIVATE_KEY":  noise_private_key_hex
    });
    if let Some(gh) = gh_token {
        string_data["GH_TOKEN"] = serde_json::json!(gh);
    }
    if let Some(mcp) = mcp_config_json {
        string_data["MCP_CONFIG_JSON"] = serde_json::json!(mcp);
    }
    let secret: Secret = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": "lair-secrets", "namespace": NAMESPACE},
        "stringData": string_data
    }))?;
    secrets.patch("lair-secrets", &PatchParams::apply("octo").force(), &Patch::Apply(secret))
        .await.context("upsert secret")?;
    Ok(())
}

pub async fn ensure_lair_pvc(client: &Client) -> anyhow::Result<()> {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), NAMESPACE);
    if pvcs.get("lair-data").await.is_ok() {
        return Ok(());
    }
    let pvc: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {"name": "lair-data", "namespace": NAMESPACE},
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "resources": {"requests": {"storage": "5Gi"}}
        }
    }))?;
    pvcs.create(&PostParams::default(), &pvc).await.context("create lair PVC")?;
    Ok(())
}

pub async fn upsert_lair_deployment(client: &Client, public_port: u16) -> anyhow::Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    let deployment: Deployment = serde_json::from_value(json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": LAIR_NAME,
            "namespace": NAMESPACE,
            "labels": {"app": LAIR_NAME}
        },
        "spec": {
            "replicas": 1,
            "selector": {"matchLabels": {"app": LAIR_NAME}},
            "template": {
                "metadata": {"labels": {"app": LAIR_NAME}},
                "spec": {
                    "serviceAccountName": LAIR_NAME,
                    "containers": [{
                        "name": LAIR_NAME,
                        "image": IMAGE,
                        "imagePullPolicy": "Always",
                        "env": [
                            {"name": "PUBLIC_PORT",           "value": public_port.to_string()},
                            {"name": "NOISE_PORT",            "value": "9000"},
                            {"name": "OCTO_DATA_DIR",    "value": "/data"},
                            {"name": "NOISE_KEY_FILE",        "value": "/data/noise_key.bin"},
                            {"name": "OCTO_SKIP_SHELL_ENV", "value": "1"}
                        ],
                        "envFrom": [{"secretRef": {"name": "lair-secrets"}}],
                        "ports": [
                            {"containerPort": 8000, "name": "http"},
                            {"containerPort": 9000, "name": "noise"}
                        ],
                        "volumeMounts": [{"name": "data", "mountPath": "/data"}]
                    }],
                    "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": "lair-data"}}]
                }
            }
        }
    }))?;
    deployments.patch(LAIR_NAME, &PatchParams::apply("octo").force(), &Patch::Apply(deployment))
        .await.context("upsert lair deployment")?;
    Ok(())
}

pub async fn ensure_lair_services(client: &Client, noise_port: u16) -> anyhow::Result<()> {
    let services: Api<Service> = Api::namespaced(client.clone(), NAMESPACE);

    let svc: Service = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": LAIR_NAME, "namespace": NAMESPACE},
        "spec": {
            "selector": {"app": LAIR_NAME},
            "ports": [{"port": 8000, "targetPort": 8000, "name": "http"}]
        }
    }))?;
    services.patch(LAIR_NAME, &PatchParams::apply("octo").force(), &Patch::Apply(svc))
        .await.context("ensure lair ClusterIP service")?;

    let np_name = format!("{LAIR_NAME}-noise");
    // NodePort value is immutable after creation; skip if the service already exists.
    if services.get(&np_name).await.is_err() {
        let np_svc: Service = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"name": np_name, "namespace": NAMESPACE},
            "spec": {
                "type": "NodePort",
                "selector": {"app": LAIR_NAME},
                "ports": [{
                    "port": 9000, "targetPort": 9000,
                    "nodePort": noise_port as i64,
                    "name": "noise"
                }]
            }
        }))?;
        services.create(&PostParams::default(), &np_svc)
            .await.context("create lair NodePort service")?;
    }

    Ok(())
}

pub async fn wait_for_deployment_ready(client: &Client, name: &str, timeout_secs: u64) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("timeout waiting for deployment '{name}' to be ready");
        }
        if let Ok(d) = deployments.get(name).await {
            if d.status.as_ref().and_then(|s| s.available_replicas).unwrap_or(0) > 0 {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

pub async fn get_running_pod(client: &Client, app_name: &str) -> anyhow::Result<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);
    let list = pods.list(&ListParams::default().labels(&format!("app={app_name}")))
        .await.context("list pods")?;
    list.into_iter()
        .find(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .and_then(|p| p.metadata.name)
        .ok_or_else(|| anyhow::anyhow!("no running pod found for app={app_name}"))
}

/// Resolve the public IP of the k8s node by running `curl ipify.org` inside a
/// pod.  This works correctly on cloud VMs (AWS, GCP, …) where the node's
/// ExternalIP is not registered in the k8s API — the pod's egress IP is the
/// node's public IP, while the CLI's own egress IP is the operator's laptop.
pub async fn get_public_ip_via_pod(client: &Client, deployment: &str) -> anyhow::Result<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);
    let lp = ListParams::default().labels(&format!("app={deployment}"));
    let list = pods.list(&lp).await.context("list pods")?;
    let pod = list.items.into_iter()
        .find(|p| {
            p.status.as_ref()
                .and_then(|s| s.phase.as_deref())
                == Some("Running")
        })
        .ok_or_else(|| anyhow::anyhow!("no Running pod found for deployment/{deployment}"))?;
    let pod_name = pod.metadata.name
        .ok_or_else(|| anyhow::anyhow!("pod has no name"))?;
    let ip = exec_in_pod(&pod_name, &["curl", "-fsSL", "--max-time", "10", "https://api.ipify.org"])
        .await
        .context("curl ipify.org inside pod")?;
    Ok(ip.trim().to_string())
}

/// Read a single key from a K8s Secret in the octo namespace.
pub async fn read_secret_value(client: &Client, secret_name: &str, key: &str) -> anyhow::Result<String> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), NAMESPACE);
    let secret  = secrets.get(secret_name).await.with_context(|| format!("get secret {secret_name}"))?;
    let bytes   = secret.data
        .ok_or_else(|| anyhow::anyhow!("secret {secret_name} has no data"))?
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("secret {secret_name} missing key '{key}'"))?
        .clone()
        .0;
    String::from_utf8(bytes).context("secret value is not UTF-8")
}

/// Trigger a rolling restart of a Deployment (equivalent to `kubectl rollout restart`).
pub async fn rollout_restart_deployment(client: &Client, name: &str) -> anyhow::Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let restart_time = format!("{secs}");
    let patch = json!({
        "spec": {
            "template": {
                "metadata": {
                    "annotations": {
                        "kubectl.kubernetes.io/restartedAt": restart_time
                    }
                }
            }
        }
    });
    deployments
        .patch(name, &PatchParams::apply("octo"), &Patch::Merge(&patch))
        .await
        .with_context(|| format!("rollout restart deployment/{name}"))?;
    Ok(())
}

// ── Pod exec (via kubectl subprocess) ─────────────────────────────────────────

/// Run a command in a pod and return its stdout.
pub async fn exec_in_pod(pod_name: &str, cmd: &[&str]) -> anyhow::Result<String> {
    let mut args = vec!["exec", pod_name, "-n", NAMESPACE, "--"];
    args.extend_from_slice(cmd);
    let out = tokio::process::Command::new("kubectl")
        .args(&args)
        .output()
        .await
        .context("kubectl exec")?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Delete the entire octo namespace, removing all resources and PVC data.
pub async fn delete_namespace(client: &Client) -> anyhow::Result<()> {
    let api: Api<Namespace> = Api::all(client.clone());
    api.delete(NAMESPACE, &DeleteParams::default()).await
        .with_context(|| format!("delete namespace {NAMESPACE}"))?;
    info!("[k8s] deleted namespace {NAMESPACE}");
    Ok(())
}

/// Returns true if the octo namespace still exists.
pub async fn namespace_exists(client: &Client) -> bool {
    let api: Api<Namespace> = Api::all(client.clone());
    api.get(NAMESPACE).await.is_ok()
}

/// Write `content` to `path` inside a running pod via `kubectl exec` stdin.
pub async fn write_pod_file(pod_name: &str, path: &str, content: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("kubectl")
        .args(["exec", "-i", pod_name, "-n", NAMESPACE, "--", "sh", "-c", &format!("cat > {path}")])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("kubectl exec write")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(content.as_bytes()).await?;
    }
    let status = child.wait().await?;
    if !status.success() {
        anyhow::bail!("kubectl exec write to {path} failed");
    }
    Ok(())
}
