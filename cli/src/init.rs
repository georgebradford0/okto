use anyhow::{Context, Result};
use octo_k8s_ops::k8s;
use data_encoding::BASE32_NOPAD;
use tokio::process::Command;

pub async fn run(
    api_key:        &str,
    gh_token:       Option<&str>,
    noise_port:     u16,
    public_port:    u16,
    mcp_config:     Option<&std::path::Path>,
    model:          Option<&str>,
    base_url:       Option<&str>,
    openai_api_key: Option<&str>,
) -> Result<()> {
    ensure_kubernetes().await?;

    let (noise_private_key_hex, pubkey_b32) = generate_keypair()?;

    let mcp_config_json: Option<String> = match mcp_config {
        None => None,
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read mcp config {}", path.display()))?;
            let mut servers: Vec<serde_json::Value> = serde_json::from_str(&text)
                .with_context(|| format!("parse mcp config {}: must be a JSON array", path.display()))?;
            // Expand ${VAR} references in env values using the local environment,
            // since the pod won't have access to the user's shell environment.
            for server in &mut servers {
                if let Some(env) = server.get_mut("env").and_then(|e| e.as_object_mut()) {
                    for (_, val) in env.iter_mut() {
                        if let Some(s) = val.as_str() {
                            if s.starts_with("${") && s.ends_with('}') {
                                let var = &s[2..s.len() - 1];
                                match std::env::var(var) {
                                    Ok(resolved) => *val = serde_json::Value::String(resolved),
                                    Err(_) => eprintln!("warning: ${{{var}}} not set in local environment — storing unexpanded"),
                                }
                            }
                        }
                    }
                }
            }
            Some(serde_json::to_string(&servers)?)
        }
    };

    let client = k8s::build_client().await?;

    println!("Ensuring octo namespace...");
    k8s::ensure_namespace(&client).await?;
    println!("Configuring RBAC...");
    k8s::ensure_rbac(&client).await?;
    println!("Storing API keys and keypair in cluster secret...");
    k8s::upsert_secret(&client, api_key, gh_token, &noise_private_key_hex, mcp_config_json.as_deref(), model, base_url, openai_api_key).await?;
    println!("Configuring GHCR image pull credentials...");
    k8s::ensure_ghcr_pull_secret(&client, gh_token).await?;
    println!("Provisioning lair data volume...");
    k8s::ensure_lair_pvc(&client).await?;
    println!("Applying lair Deployment...");
    k8s::upsert_lair_deployment(&client, public_port).await?;
    println!("Configuring ClusterIP and NodePort services...");
    k8s::ensure_lair_services(&client, noise_port).await?;

    if public_port != noise_port {
        println!("Setting up socat proxy ({public_port} -> {noise_port})...");
        ensure_socat_proxy(public_port, noise_port).await?;
    }

    // Restart so the pod loads the new keypair from the secret before we print the QR.
    println!("Restarting lair to load new keypair...");
    k8s::rollout_restart_deployment(&client, "lair").await?;
    println!("Waiting for lair to be ready...");
    k8s::wait_for_deployment_ready(&client, "lair", 180).await?;

    let ip = k8s::get_public_ip_via_pod(&client, "lair").await?;
    let qr_data = format!("2:{ip}:{public_port}:{pubkey_b32}");

    println!("\nlair is live at {ip} (Noise NodePort {noise_port}, QR port {public_port})");
    println!("QR data: {qr_data}\n");

    let code = qrcode::QrCode::new(&qr_data).context("generate QR code")?;
    let image = code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Dark)
        .light_color(qrcode::render::unicode::Dense1x2::Light)
        .build();
    println!("{image}");

    Ok(())
}

fn generate_keypair() -> Result<(String, String)> {
    println!("Generating Noise_XX_25519 keypair...");
    let builder = snow::Builder::new(
        "Noise_XX_25519_ChaChaPoly_SHA256".parse().context("parse noise params")?,
    );
    let keypair = builder.generate_keypair().context("generate keypair")?;
    let mut combined = keypair.private.clone();
    combined.extend_from_slice(&keypair.public);
    Ok((hex::encode(&combined), BASE32_NOPAD.encode(&keypair.public)))
}

#[cfg(target_os = "linux")]
async fn ensure_socat_proxy(public_port: u16, noise_port: u16) -> Result<()> {
    // Install socat if missing.
    let has_socat = Command::new("which").arg("socat").output().await
        .map(|o| o.status.success()).unwrap_or(false);
    if !has_socat {
        run_sh("apt-get install -y socat").await?;
    }

    let unit = format!(
        "[Unit]\nDescription=Noise TCP proxy {public_port} -> {noise_port}\nAfter=network.target\n\n\
         [Service]\nExecStart=/usr/bin/socat TCP-LISTEN:{public_port},fork,reuseaddr TCP:127.0.0.1:{noise_port}\n\
         Restart=always\nRestartSec=3\n\n\
         [Install]\nWantedBy=multi-user.target\n"
    );
    run_sh(&format!(
        "echo '{}' | sudo tee /etc/systemd/system/noise-proxy.service > /dev/null",
        unit.replace('\'', "'\\''")
    )).await.context("write noise-proxy.service")?;
    run_sh("sudo systemctl daemon-reload && sudo systemctl enable --now noise-proxy").await?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn ensure_socat_proxy(_public_port: u16, _noise_port: u16) -> Result<()> {
    println!("  (skipping socat setup — not Linux; proxy must be configured manually)");
    Ok(())
}

async fn ensure_kubernetes() -> Result<()> {
    if !kubectl_available().await {
        install_kubectl().await?;
    }
    if !cluster_reachable().await {
        install_k3s().await?;
    }
    Ok(())
}

async fn kubectl_available() -> bool {
    Command::new("kubectl")
        .args(["version", "--client", "--output=json"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn cluster_reachable() -> bool {
    Command::new("kubectl")
        .arg("cluster-info")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
async fn run_sh(cmd: &str) -> Result<()> {
    let status = Command::new("sh")
        .args(["-c", cmd])
        .status()
        .await
        .context("sh")?;
    if !status.success() {
        anyhow::bail!("command failed: {cmd}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn install_kubectl() -> Result<()> {
    println!("kubectl not found, installing...");
    let arch = if cfg!(target_arch = "x86_64") { "amd64" } else { "arm64" };
    run_sh(&format!(
        r#"set -e
VER=$(curl -fsSL https://dl.k8s.io/release/stable.txt)
curl -fsSL "https://dl.k8s.io/release/$VER/bin/linux/{arch}/kubectl" -o /tmp/kubectl
sudo install -o root -g root -m 0755 /tmp/kubectl /usr/local/bin/kubectl
rm /tmp/kubectl"#
    ))
    .await?;
    println!("kubectl installed.");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn install_kubectl() -> Result<()> {
    anyhow::bail!(
        "kubectl not found.\n  macOS:  brew install kubectl\n  Other:  https://kubernetes.io/docs/tasks/tools/"
    )
}

#[cfg(target_os = "linux")]
async fn install_k3s() -> Result<()> {
    println!("No Kubernetes cluster found, installing k3s...");
    // --write-kubeconfig-mode=644 makes the kubeconfig readable by non-root users.
    run_sh("curl -sfL https://get.k3s.io | INSTALL_K3S_EXEC='--write-kubeconfig-mode=644' sh -").await?;

    let kubeconfig = "/etc/rancher/k3s/k3s.yaml";
    std::env::set_var("KUBECONFIG", kubeconfig);

    println!("Waiting for k3s to be ready...");
    for i in 0..60 {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        if cluster_reachable().await {
            println!("k3s is ready.");
            println!("  Add to your shell: export KUBECONFIG={kubeconfig}");
            return Ok(());
        }
        if i > 0 && i % 5 == 0 {
            println!("  Still waiting... ({}s)", (i + 1) * 3);
        }
    }
    anyhow::bail!("k3s installed but cluster did not become reachable within 180s")
}

#[cfg(not(target_os = "linux"))]
async fn install_k3s() -> Result<()> {
    anyhow::bail!(
        "No Kubernetes cluster reachable.\n\
         Options:\n\
         • Docker Desktop: enable Kubernetes in settings\n\
         • k3d:      brew install k3d && k3d cluster create\n\
         • minikube: brew install minikube && minikube start"
    )
}
