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

/// Persistent management API token (`X-Octo-Token` header value). Generated
/// on first run, chmod 0600, passed to the lair container via
/// `docker -e LAIR_MGMT_TOKEN=<value>`. Children never see it — lair's
/// `agent_proc::spawn` strips the env var before exec.
pub fn mgmt_token_path() -> PathBuf { lair_data_dir().join(".mgmt-token") }

/// Read `~/.octo/lair/.mgmt-token`, generating it (random 64 hex chars,
/// chmod 0600) on first call.
pub fn ensure_mgmt_token() -> Result<String> {
    let path = mgmt_token_path();
    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let token = random_hex(32);
    fs::write(&path, &token)
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms).ok();
    }
    Ok(token)
}

/// Read the management token if it exists. Returns `None` if the file is
/// missing or empty.
pub fn read_mgmt_token() -> Option<String> {
    fs::read_to_string(mgmt_token_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    // `/dev/urandom` is the standard kernel CSPRNG on Linux and is fine for
    // a long-lived shared secret; no need to pull in a rand crate.
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("read /dev/urandom for mgmt token");
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

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

/// Run a command inside the lair container via `docker exec` and return its
/// exit status. The container must already be running. Used by callers that
/// only care whether the command succeeded (e.g. `command -v` probes).
pub fn docker_exec_status(args: &[&str]) -> Result<std::process::ExitStatus> {
    let mut full: Vec<&str> = vec!["exec", LAIR_CONTAINER_NAME];
    full.extend_from_slice(args);
    std::process::Command::new("docker")
        .args(&full)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("spawn `docker exec {LAIR_CONTAINER_NAME} {}`", args.join(" ")))
}

/// Read the last `tail` lines of `docker logs <LAIR_CONTAINER_NAME>` as a
/// single string. Used by callers that scan startup-time markers (MCP
/// connect / spawn-fail) without needing to stream.
pub fn read_lair_logs(tail: u32) -> Result<String> {
    let tail_arg = tail.to_string();
    let out = docker_output(&["logs", "--tail", &tail_arg, LAIR_CONTAINER_NAME])?;
    // `docker logs` writes container stdout to its own stdout and container
    // stderr to its own stderr; combine both so the caller doesn't have to
    // know which stream a given marker landed in.
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(s)
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

    // Management API bearer token. Minted on first call, persisted to
    // `~/.octo/lair/.mgmt-token` (chmod 0600), and supplied to lair via
    // `-e LAIR_MGMT_TOKEN=...`. Children never see it — lair strips the env
    // var before spawning them, and they run as a different uid so
    // `/proc/1/environ` is also inaccessible.
    let mgmt_token = ensure_mgmt_token()?;

    let noise_port = launch.noise_port.to_string();
    let http_port  = launch.http_port.to_string();
    let publish_noise = format!("{noise_port}:8443");
    // Loopback-only on the host: the management API is for the CLI, never
    // exposed publicly.
    let publish_http  = format!("127.0.0.1:{http_port}:8000");
    let mount = format!("{}:/data", launch.config_dir.display());
    let env_file_arg = launch.env_file.display().to_string();
    let public_port = format!("PUBLIC_PORT={noise_port}");
    let mgmt_env    = format!("LAIR_MGMT_TOKEN={mgmt_token}");

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
        // Token for the CLI ↔ lair management API. Set last so we can be
        // sure it overrides any LAIR_MGMT_TOKEN that snuck into the env
        // file (the parser of which doesn't reject it as a managed key).
        "-e",           &mgmt_env,
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
