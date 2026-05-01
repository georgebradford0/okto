use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{Node, PersistentVolumeClaim, Secret, Service},
};
use kube::{
    api::{DeleteParams, ListParams, Patch, PatchParams, PostParams},
    Api, Client,
};
use serde_json::json;
use tracing::{error, info};

pub const NAMESPACE: &str = "claudulhu";
const NODEPORT_MIN: u16 = 30100;
const NODEPORT_MAX: u16 = 30199;
const IMAGE: &str = "ghcr.io/georgebradford0/rulyeh:latest";
const ENTRYPOINT: &str = "/usr/local/bin/docker-entrypoint-server.sh";

pub struct ChildInfo {
    pub name: String,
    pub git_url: String,
    pub status: String,
    pub noise_port: u16,
    pub remote: bool,
    pub instance_id: Option<String>,
}

pub async fn build_client() -> anyhow::Result<Client> {
    Ok(Client::try_default().await?)
}

pub async fn list_managed_deployments(client: &Client) -> anyhow::Result<Vec<ChildInfo>> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    let list = deployments
        .list(&ListParams::default().labels("claudulhu.managed=1"))
        .await
        .context("list deployments")?;

    let services: Api<Service> = Api::namespaced(client.clone(), NAMESPACE);
    let mut results = Vec::new();

    for d in list {
        let name = d.metadata.name.unwrap_or_default();
        let labels      = d.metadata.labels.unwrap_or_default();
        let annotations = d.metadata.annotations.unwrap_or_default();
        let git_url     = annotations.get("claudulhu.git_url").cloned().unwrap_or_default();
        let remote      = labels.get("claudulhu.remote").map(|v| v == "1").unwrap_or(false);
        let instance_id = labels.get("claudulhu.ec2-instance-id").cloned();

        let status = {
            let st = d.status.as_ref();
            let available = st.and_then(|s| s.available_replicas).unwrap_or(0);
            let replicas = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0);
            if available > 0 {
                "running"
            } else if replicas == 0 {
                "stopped"
            } else {
                "pending"
            }
            .to_string()
        };

        let noise_port = match services.get(&format!("{name}-noise")).await {
            Ok(svc) => svc
                .spec
                .as_ref()
                .and_then(|s| s.ports.as_ref())
                .and_then(|ports| ports.first())
                .and_then(|p| p.node_port)
                .map(|p| p as u16)
                .unwrap_or(NODEPORT_MIN),
            Err(_) => NODEPORT_MIN,
        };

        results.push(ChildInfo {
            name,
            git_url,
            status,
            noise_port,
            remote,
            instance_id,
        });
    }

    Ok(results)
}


pub async fn assign_nodeport(client: &Client) -> anyhow::Result<u16> {
    let services: Api<Service> = Api::namespaced(client.clone(), NAMESPACE);
    let list = services.list(&ListParams::default()).await.context("list services")?;

    let used: std::collections::HashSet<u16> = list
        .iter()
        .filter(|s| s.metadata.name.as_deref().map(|n| n.ends_with("-noise")).unwrap_or(false))
        .flat_map(|s| {
            s.spec
                .as_ref()
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
    let token = secret
        .data
        .and_then(|d| d.get("token").cloned())
        .map(|b| String::from_utf8(b.0))
        .transpose()
        .context("decode join token")?
        .ok_or_else(|| anyhow::anyhow!("k3s-join-token secret missing 'token' key"))?;
    Ok(token)
}

pub struct CreateChildParams<'a> {
    pub name: &'a str,
    pub git_url: Option<&'a str>,
    pub noise_port: u16,
    pub api_key: &'a str,
    pub gh_token: Option<&'a str>,
    pub pub_host: &'a str,
    pub rulyeh_url: &'a str,
    pub startup_script: Option<&'a str>,
    pub startup_prompt: Option<&'a str>,
    pub node_selector: Option<HashMap<String, String>>,
    pub remote: bool,
    pub instance_id: Option<&'a str>,
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

    let mut meta_labels = json!({
        "claudulhu.managed": "1",
        "app": p.name,
    });
    if p.remote {
        meta_labels["claudulhu.remote"] = json!("1");
    }
    if let Some(iid) = p.instance_id {
        meta_labels["claudulhu.ec2-instance-id"] = json!(iid);
    }

    // git_url is stored as an annotation (no format restrictions) rather than a
    // label — URLs contain '/' and ':' which are invalid in label values.
    let mut meta_annotations = json!({});
    if let Some(url) = p.git_url {
        meta_annotations["claudulhu.git_url"] = json!(url);
    }

    let mut env = vec![
        json!({"name": "ANTHROPIC_API_KEY",  "value": p.api_key}),
        json!({"name": "NOISE_PORT",         "value": "9000"}),
        json!({"name": "PUBLIC_PORT",        "value": p.noise_port.to_string()}),
        json!({"name": "PUBLIC_HOST",        "value": p.pub_host}),
        json!({"name": "RULYEH_URL",         "value": p.rulyeh_url}),
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
    if std::env::var("CLAUDULHU_DEV").as_deref() == Ok("1") {
        env.push(json!({"name": "CLAUDULHU_DEV", "value": "1"}));
    }

    let pull_policy = if std::env::var("CLAUDULHU_DEV").as_deref() == Ok("1") {
        "IfNotPresent"
    } else {
        "Always"
    };

    let data_pvc = format!("{}-data", p.name);
    let workspace_pvc = format!("{}-workspace", p.name);

    let mut pod_spec = json!({
        "containers": [{
            "name": "claudulhu",
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
    if let Some(ns) = &p.node_selector {
        pod_spec["nodeSelector"] = serde_json::to_value(ns)?;
    }

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

    deployments
        .create(&PostParams::default(), &deployment)
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

/// Rollout-restart one or more Deployments by patching the `kubectl.kubernetes.io/restartedAt`
/// pod-template annotation.  Pass `names = &[]` to restart **all** managed children plus rulyeh
/// itself; otherwise only the named deployments are restarted.
pub async fn restart_deployments(client: &Client, names: &[&str]) -> anyhow::Result<Vec<String>> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    let now = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        // Format as RFC 3339 UTC (kubectl accepts this format)
        let s = secs % 60;
        let m = (secs / 60) % 60;
        let h = (secs / 3600) % 24;
        let mut remaining_days = secs / 86400;
        // Simple date calculation from Unix epoch
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
        "spec": {
            "template": {
                "metadata": {
                    "annotations": {
                        "kubectl.kubernetes.io/restartedAt": now
                    }
                }
            }
        }
    });

    let targets: Vec<String> = if names.is_empty() {
        // All managed children + rulyeh itself
        let list = deployments
            .list(&ListParams::default().labels("claudulhu.managed=1"))
            .await
            .context("list managed deployments")?;
        let mut t: Vec<String> = list
            .iter()
            .filter_map(|d| d.metadata.name.clone())
            .collect();
        t.push("rulyeh".to_string());
        t
    } else {
        names.iter().map(|s| s.to_string()).collect()
    };

    let mut restarted = Vec::new();
    for name in &targets {
        match deployments
            .patch(name, &PatchParams::default(), &Patch::Merge(patch.clone()))
            .await
        {
            Ok(_) => {
                info!("[k8s] restarted Deployment {name}");
                restarted.push(name.clone());
            }
            Err(e) => {
                error!("[k8s] restart Deployment {name} failed: {e}");
            }
        }
    }
    Ok(restarted)
}

pub async fn scale_deployment(client: &Client, name: &str, replicas: i32) -> anyhow::Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    deployments
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(json!({"spec": {"replicas": replicas}})),
        )
        .await
        .with_context(|| format!("scale Deployment {name} to {replicas}"))?;
    info!("[k8s] scaled Deployment {name} to {replicas} replica(s)");
    Ok(())
}

pub async fn delete_child_resources(
    client: &Client,
    name: &str,
    node_name: Option<&str>,
    instance_id: Option<&str>,
) -> anyhow::Result<()> {
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
    pvcs.delete(&format!("{name}-data"), &dp).await.ok();
    pvcs.delete(&format!("{name}-workspace"), &dp).await.ok();
    info!("[k8s] deleted PVCs for {name}");

    if let Some(iid) = instance_id {
        if let Err(e) = crate::aws::terminate_instance(iid).await {
            error!("[k8s] terminate EC2 {iid}: {e}");
        }
    }

    if let Some(node) = node_name {
        let nodes: Api<Node> = Api::all(client.clone());
        nodes.delete(node, &dp).await.ok();
        info!("[k8s] deleted Node {node}");
    }

    Ok(())
}

pub async fn find_node_for_child(client: &Client, child_name: &str) -> Option<String> {
    let nodes: Api<Node> = Api::all(client.clone());
    nodes
        .list(&ListParams::default().labels(&format!("claudulhu.child-name={child_name}")))
        .await
        .ok()?
        .into_iter()
        .next()
        .and_then(|n| n.metadata.name)
}

pub async fn wait_for_node_ready(
    client: &Client,
    child_name: &str,
    timeout_secs: u64,
) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("timeout waiting for node with label claudulhu.child-name={child_name}");
        }
        let nodes: Api<Node> = Api::all(client.clone());
        let list = nodes
            .list(&ListParams::default().labels(&format!("claudulhu.child-name={child_name}")))
            .await?;
        for node in &list {
            let ready = node
                .status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .and_then(|conds| conds.iter().find(|c| c.type_ == "Ready"))
                .map(|c| c.status == "True")
                .unwrap_or(false);
            if ready {
                let node_name = node.metadata.name.clone().unwrap_or_default();
                info!("[k8s] node {node_name} is Ready (child={child_name})");
                return Ok(node_name);
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

pub async fn label_node(
    client: &Client,
    node_name: &str,
    labels: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let nodes: Api<Node> = Api::all(client.clone());
    nodes
        .patch(
            node_name,
            &PatchParams::default(),
            &Patch::Merge(json!({"metadata": {"labels": labels}})),
        )
        .await
        .with_context(|| format!("label node {node_name}"))?;
    info!("[k8s] labeled node {node_name}");
    Ok(())
}
