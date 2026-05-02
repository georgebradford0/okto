use anyhow::{Context, Result};
use claudulhu_k8s_ops::k8s;
use data_encoding::BASE32_NOPAD;
use tokio::process::Command;

pub async fn run(api_key: &str, gh_token: Option<&str>, noise_port: u16) -> Result<()> {
    ensure_kubernetes().await?;

    let (noise_private_key_hex, pubkey_b32) = generate_keypair()?;

    let client = k8s::build_client().await?;

    println!("→ namespace");
    k8s::ensure_namespace(&client).await?;
    println!("→ RBAC");
    k8s::ensure_rbac(&client).await?;
    println!("→ secrets");
    k8s::upsert_secret(&client, api_key, gh_token, &noise_private_key_hex).await?;
    println!("→ PVC");
    k8s::ensure_rulyeh_pvc(&client).await?;
    println!("→ deployment");
    k8s::upsert_rulyeh_deployment(&client, noise_port).await?;
    println!("→ services");
    k8s::ensure_rulyeh_services(&client, noise_port).await?;
    // Restart so the pod loads the new keypair from the secret before we print the QR.
    println!("→ restarting rulyeh to apply new keypair...");
    k8s::rollout_restart_deployment(&client, "rulyeh").await?;
    println!("→ waiting for rulyeh to be ready...");
    k8s::wait_for_deployment_ready(&client, "rulyeh", 180).await?;

    let ip = k8s::get_public_ip_via_pod(&client, "rulyeh").await?;
    let qr_data = format!("2:{ip}:{noise_port}:{pubkey_b32}");

    println!("\nrulyeh is live at {ip}:{noise_port}");
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
    println!("→ generating new Noise keypair");
    let builder = snow::Builder::new(
        "Noise_XX_25519_ChaChaPoly_SHA256".parse().context("parse noise params")?,
    );
    let keypair = builder.generate_keypair().context("generate keypair")?;
    let mut combined = keypair.private.clone();
    combined.extend_from_slice(&keypair.public);
    Ok((hex::encode(&combined), BASE32_NOPAD.encode(&keypair.public)))
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
curl -fsSL "https://dl.k8s.io/release/$VER/bin/linux/{arch}/kubectl" -o /usr/local/bin/kubectl
chmod +x /usr/local/bin/kubectl"#
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
    run_sh("curl -sfL https://get.k3s.io | sh -").await?;

    // k3s writes its kubeconfig to /etc/rancher/k3s/k3s.yaml
    let kubeconfig = "/etc/rancher/k3s/k3s.yaml";
    std::env::set_var("KUBECONFIG", kubeconfig);

    println!("Waiting for k3s to be ready...");
    for i in 0..30 {
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
    anyhow::bail!("k3s installed but cluster did not become reachable within 90s")
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
