//! Lair container management on the operator's host.
//!
//! The lair binary ships as a multi-arch docker image
//! (`ghcr.io/georgebradford0/octo-lair`). The CLI never imports a Docker SDK
//! — every interaction shells out to the `docker` CLI on the operator's
//! PATH. This module wraps the few invocations the CLI needs: run / rm /
//! inspect / pull / logs.
//!
//! Inside the container, lair and every child agent it spawns are plain
//! processes; the operator's host `~/.octo` is bind-mounted at `/data` so
//! `config.json`, `lair/`, and `agents/` stay visible to the CLI.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result};

pub const LAIR_DEFAULT_HTTP_PORT:  u16 = 8000;
pub const LAIR_DEFAULT_NOISE_PORT: u16 = 8443;

/// Container name the CLI uses for the lair instance on this host. Used as
/// the `--name` flag on `docker run` and as the target for every subsequent
/// `docker rm`, `docker inspect`, `docker logs`, etc.
pub const LAIR_CONTAINER_NAME: &str = "octo-lair";

/// Default image reference. Override via `$OCTO_LAIR_IMAGE` or stored in
/// `lair-launch.json` (`image` field).
pub const DEFAULT_LAIR_IMAGE: &str = "ghcr.io/georgebradford0/octo-lair:latest";

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

/// Operator's config dir. Always `$HOME/.octo`. Mounted into the container
/// at `/data`.
pub fn config_dir() -> PathBuf { home_dir().join(".octo") }

/// Lair's per-host data dir. Lives at `<config_dir>/lair` on the host and
/// `/data/lair` inside the container.
pub fn lair_data_dir() -> PathBuf { config_dir().join("lair") }

/// Per-agent dirs root. `<config_dir>/agents` on the host, `/data/agents`
/// inside the container.
pub fn agents_dir() -> PathBuf { config_dir().join("agents") }

/// Operator-supplied env vars passed into the lair container (one KEY=VALUE
/// per line). Mounted as `docker --env-file`.
pub fn env_file_path() -> PathBuf { config_dir().join("lair-env") }

/// Bookkeeping for `octo reload` — records the ports and image tag passed to
/// the most recent `octo init`.
pub fn launch_config_path() -> PathBuf { config_dir().join("lair-launch.json") }

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct LaunchRecord {
    pub noise_port: u16,
    pub http_port:  u16,
    /// Image reference used the last time lair was started. Carried forward
    /// across `octo reload` so the operator doesn't have to repass it.
    #[serde(default)]
    pub image:      Option<String>,
}

pub fn write_launch(rec: &LaunchRecord) -> Result<()> {
    let path = launch_config_path();
    fs::create_dir_all(path.parent().unwrap()).ok();
    let body = serde_json::to_string_pretty(rec).context("encode lair-launch.json")?;
    fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn read_launch() -> Option<LaunchRecord> {
    fs::read_to_string(launch_config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// Resolve the lair image reference. Precedence: `$OCTO_LAIR_IMAGE` →
/// `lair-launch.json` → `DEFAULT_LAIR_IMAGE`.
pub fn resolve_image(launch_image: Option<&str>) -> String {
    if let Ok(v) = std::env::var("OCTO_LAIR_IMAGE") {
        if !v.is_empty() { return v; }
    }
    if let Some(img) = launch_image.filter(|s| !s.is_empty()) {
        return img.to_string();
    }
    DEFAULT_LAIR_IMAGE.to_string()
}

fn which(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").ok_or_else(|| anyhow::anyhow!("PATH not set"))?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("'{name}' not found on PATH")
}

/// Verify `docker` is on PATH. Called from every CLI entry point that drives
/// the lair container; we'd rather fail fast with a clear message than have
/// `start_lair` blow up in the middle of an init.
pub fn ensure_docker_present() -> Result<()> {
    which("docker")
        .map(|_| ())
        .context("`docker` is required on PATH. Install Docker Engine (https://docs.docker.com/engine/install/) and try again.")
}

fn docker_status(args: &[&str]) -> Result<std::process::ExitStatus> {
    std::process::Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("spawn `docker {}`", args.join(" ")))
}

fn docker_output(args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn `docker {}`", args.join(" ")))
}

/// True if a container named `LAIR_CONTAINER_NAME` is running on this host.
pub fn is_running() -> bool {
    let out = match docker_output(&[
        "inspect", "-f", "{{.State.Running}}", LAIR_CONTAINER_NAME,
    ]) {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !out.status.success() {
        return false;
    }
    String::from_utf8_lossy(&out.stdout).trim() == "true"
}

/// True if a container with the lair name exists at all (running or stopped).
fn container_exists() -> bool {
    docker_status(&["inspect", LAIR_CONTAINER_NAME])
        .map(|s| s.success())
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
pub struct LairLaunch<'a> {
    pub noise_port: u16,
    pub http_port:  u16,
    pub config_dir: &'a Path,
    pub env_file:   &'a Path,
    pub image:      &'a str,
}

/// Stop any existing lair container, then `docker run` a fresh one in
/// detached mode. Returns the container's short ID.
///
/// The container is named `octo-lair` and bind-mounts the operator's
/// `~/.octo` at `/data`. Env vars from the lair-env file are forwarded
/// through `--env-file`; the file shape is plain `KEY=VALUE` per line which
/// docker reads verbatim.
pub fn start_lair(launch: &LairLaunch<'_>) -> Result<String> {
    ensure_docker_present()?;
    stop_lair_if_running();

    fs::create_dir_all(launch.config_dir).ok();
    fs::create_dir_all(launch.config_dir.join("lair")).ok();
    fs::create_dir_all(launch.config_dir.join("agents")).ok();

    // docker errors out on a missing --env-file, so make sure it exists even
    // if the operator hasn't run `octo env set` yet.
    if !launch.env_file.exists() {
        fs::write(launch.env_file, "").ok();
    }

    let noise_port = launch.noise_port.to_string();
    let http_port  = launch.http_port.to_string();
    let publish_noise = format!("{noise_port}:8443");
    // Loopback-only on the host: the management API is for the CLI, never
    // exposed publicly.
    let publish_http  = format!("127.0.0.1:{http_port}:8000");
    let mount = format!("{}:/data", launch.config_dir.display());
    let env_file_arg = launch.env_file.display().to_string();
    let public_port = format!("PUBLIC_PORT={noise_port}");

    let args: Vec<&str> = vec![
        "run", "-d",
        "--name",       LAIR_CONTAINER_NAME,
        "--restart",    "unless-stopped",
        "-p",           &publish_noise,
        "-p",           &publish_http,
        "-v",           &mount,
        "--env-file",   &env_file_arg,
        // Tell lair which port to advertise in the QR code. NOISE_PORT inside
        // the container is hardcoded to 8443 (the EXPOSE'd port); PUBLIC_PORT
        // is what the mobile client should connect to from outside.
        "-e",           &public_port,
        launch.image,
    ];

    let out = docker_output(&args)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("docker run failed: {stderr}");
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(id)
}

/// Stop + remove the lair container if one exists. Idempotent.
pub fn stop_lair_if_running() {
    if !container_exists() { return; }
    let _ = docker_status(&["rm", "-f", LAIR_CONTAINER_NAME]);
}

/// `docker pull <image>`. Used by `octo lair update`.
pub fn docker_pull(image: &str) -> Result<()> {
    ensure_docker_present()?;
    let status = std::process::Command::new("docker")
        .args(["pull", image])
        .status()
        .with_context(|| format!("spawn `docker pull {image}`"))?;
    if !status.success() {
        anyhow::bail!("`docker pull {image}` exited with {status}");
    }
    Ok(())
}

/// Wait for `http://127.0.0.1:<port>/health` to return 200, up to `timeout`.
pub async fn wait_for_health(port: u16, timeout: Duration) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}/health");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("lair did not become ready within {:?}", timeout);
        }
        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => return Ok(()),
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

pub async fn detect_public_ip() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let resp = client.get("https://api.ipify.org").send().await
        .context("detect public IP via api.ipify.org")?;
    let body = resp.text().await.context("read ipify body")?;
    Ok(body.trim().to_string())
}

/// CLI ↔ lair management API base URL. Lair binds HTTP on the container's
/// 0.0.0.0:8000; the CLI hits the host-published 127.0.0.1:<http_port>.
pub fn lair_http_url() -> String {
    let port = read_launch().map(|r| r.http_port).unwrap_or(LAIR_DEFAULT_HTTP_PORT);
    format!("http://127.0.0.1:{port}")
}

/// Stream `docker logs` for the lair container into stdout. `follow` maps
/// straight to `-f`; tail defaults to the last 1k lines to mirror the old
/// 1MB-from-file behaviour without scanning a long-running container's
/// entire stdout.
pub async fn stream_lair_logs(follow: bool) -> Result<()> {
    ensure_docker_present()?;
    let mut cmd = std::process::Command::new("docker");
    cmd.args(["logs", "--tail", "1000"]);
    if follow { cmd.arg("--follow"); }
    cmd.arg(LAIR_CONTAINER_NAME);
    let status = cmd.status().context("spawn `docker logs`")?;
    if !status.success() {
        anyhow::bail!("`docker logs {LAIR_CONTAINER_NAME}` exited with {status}");
    }
    Ok(())
}
