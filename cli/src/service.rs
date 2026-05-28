//! Lair container management on the operator's host.
//!
//! The lair binary ships as a multi-arch docker image
//! (`ghcr.io/georgebradford0/lair`). The CLI never imports a Docker SDK
//! — every interaction shells out to the `docker` CLI on the operator's
//! PATH. This module wraps the few invocations the CLI needs: run / rm /
//! inspect / pull / logs.
//!
//! Inside the container, lair and every child agent it spawns are plain
//! processes; the operator's host `~/.okto` is bind-mounted at `/data` so
//! `config.json`, `lair/`, and `agents/` stay visible to the CLI.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

pub const LAIR_DEFAULT_HTTP_PORT:  u16 = 8000;
pub const LAIR_DEFAULT_NOISE_PORT: u16 = 8443;

/// Container name the CLI uses for the lair instance on this host. Used as
/// the `--name` flag on `docker run` and as the target for every subsequent
/// `docker rm`, `docker inspect`, `docker logs`, etc.
pub const LAIR_CONTAINER_NAME: &str = "lair";

/// Default image reference. Override via `$OKTO_LAIR_IMAGE` or stored in
/// `lair-launch.json` (`image` field).
pub const DEFAULT_LAIR_IMAGE: &str = "ghcr.io/georgebradford0/lair:latest";

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

/// Operator's config dir. Always `$HOME/.okto`. Mounted into the container
/// at `/data`.
pub fn config_dir() -> PathBuf { home_dir().join(".okto") }

/// Lair's per-host data dir. Lives at `<config_dir>/lair` on the host and
/// `/data/lair` inside the container.
pub fn lair_data_dir() -> PathBuf { config_dir().join("lair") }

/// Per-agent dirs root. `<config_dir>/agents` on the host, `/data/agents`
/// inside the container.
pub fn agents_dir() -> PathBuf { config_dir().join("agents") }

/// Operator-supplied env vars passed into the lair container (one KEY=VALUE
/// per line). Mounted as `docker --env-file`.
pub fn env_file_path() -> PathBuf { config_dir().join("lair-env") }

/// Bookkeeping for `okto reload` — records the ports and image tag passed to
/// the most recent `okto init`.
pub fn launch_config_path() -> PathBuf { config_dir().join("lair-launch.json") }

/// Bundled seccomp profile path. Written on first `start_lair` so docker run
/// can pass `--security-opt seccomp=<path>` against it.
pub fn seccomp_profile_path() -> PathBuf { config_dir().join("seccomp.json") }

/// Docker's default seccomp profile with a single rule added at the top that
/// allows `unshare` / `setns` / `clone3` / `clone` unconditionally (Docker's
/// default gates them on `CAP_SYS_ADMIN`, which our non-root agent uids don't
/// have — so `unshare(CLONE_NEWUSER)` returns EPERM and rootless container
/// builders fail at startup). Operator-editable once written; the CLI won't
/// overwrite an existing file.
const SECCOMP_PROFILE: &str = include_str!("../seccomp.json");

/// Write the bundled seccomp profile to `seccomp_profile_path()` if it
/// doesn't already exist. Called from `start_lair` so every code path that
/// (re)starts lair populates the file before `docker run` references it.
pub fn ensure_seccomp_profile() -> Result<PathBuf> {
    let path = seccomp_profile_path();
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&path, SECCOMP_PROFILE)
            .with_context(|| format!("write seccomp profile {}", path.display()))?;
        info!("[service] wrote bundled seccomp profile to {}", path.display());
    }
    Ok(path)
}

/// Persistent management API token (`X-Okto-Token` header value). Generated
/// on first run, chmod 0600, passed to the lair container via
/// `docker -e LAIR_MGMT_TOKEN=<value>`. Children never see it — lair's
/// `agent_proc::spawn` strips the env var before exec.
pub fn mgmt_token_path() -> PathBuf { lair_data_dir().join(".mgmt-token") }

/// Read `~/.okto/lair/.mgmt-token`, generating it (random 64 hex chars,
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
    debug!("[service] minting new management token at {}", path.display());
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
    /// across `okto reload` so the operator doesn't have to repass it.
    #[serde(default)]
    pub image:      Option<String>,
}

pub fn write_launch(rec: &LaunchRecord) -> Result<()> {
    let path = launch_config_path();
    fs::create_dir_all(path.parent().unwrap()).ok();
    let body = serde_json::to_string_pretty(rec).context("encode lair-launch.json")?;
    fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    debug!("[service] wrote launch record {}", path.display());
    Ok(())
}

pub fn read_launch() -> Option<LaunchRecord> {
    fs::read_to_string(launch_config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// Resolve the lair image reference. Precedence: `$OKTO_LAIR_IMAGE` →
/// `lair-launch.json` → `DEFAULT_LAIR_IMAGE`.
pub fn resolve_image(launch_image: Option<&str>) -> String {
    if let Ok(v) = std::env::var("OKTO_LAIR_IMAGE") {
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
    debug!("[service] shelling out: docker {}", args.join(" "));
    let status = std::process::Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("spawn `docker {}`", args.join(" ")))?;
    debug!("[service] `docker {}` exited with {status}", args.join(" "));
    Ok(status)
}

fn docker_output(args: &[&str]) -> Result<std::process::Output> {
    debug!("[service] shelling out: docker {}", args.join(" "));
    let out = std::process::Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn `docker {}`", args.join(" ")))?;
    if out.status.success() {
        debug!("[service] `docker {}` exited with {}", args.join(" "), out.status);
    } else {
        debug!(
            "[service] `docker {}` exited with {}: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
    Ok(out)
}

/// Run a command inside the lair container via `docker exec` and return its
/// exit status. The container must already be running. Used by callers that
/// only care whether the command succeeded (e.g. `command -v` probes).
pub fn docker_exec_status(args: &[&str]) -> Result<std::process::ExitStatus> {
    let mut full: Vec<&str> = vec!["exec", LAIR_CONTAINER_NAME];
    full.extend_from_slice(args);
    debug!("[service] shelling out: docker {}", full.join(" "));
    let status = std::process::Command::new("docker")
        .args(&full)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("spawn `docker exec {LAIR_CONTAINER_NAME} {}`", args.join(" ")))?;
    debug!("[service] `docker exec {LAIR_CONTAINER_NAME} {}` exited with {status}", args.join(" "));
    Ok(status)
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

/// Run `lair --version` inside the running lair container and return its
/// trimmed stdout (e.g. `lair 0.12.0`). Errors if the container isn't
/// running. Reports the actual binary baked into the image rather than the
/// image tag, which may just be `:latest`.
pub fn lair_binary_version() -> Result<String> {
    ensure_docker_present()?;
    if !is_running() {
        anyhow::bail!("lair is not running. Run `okto init` or `okto reload` first.");
    }
    let out = docker_output(&["exec", LAIR_CONTAINER_NAME, "lair", "--version"])?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        error!("[service] `lair --version` exited with {}: {stderr}", out.status);
        anyhow::bail!("`lair --version` exited with {}: {stderr}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
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
/// The container is named `lair` and bind-mounts the operator's
/// `~/.okto` at `/data`. Env vars from the lair-env file are forwarded
/// through `--env-file`; the file shape is plain `KEY=VALUE` per line which
/// docker reads verbatim.
pub fn start_lair(launch: &LairLaunch<'_>) -> Result<String> {
    ensure_docker_present()?;
    stop_lair_if_running();

    fs::create_dir_all(launch.config_dir).ok();
    fs::create_dir_all(launch.config_dir.join("lair")).ok();
    fs::create_dir_all(launch.config_dir.join("agents")).ok();

    // docker errors out on a missing --env-file, so make sure it exists even
    // if the operator hasn't run `okto env set` yet.
    if !launch.env_file.exists() {
        debug!("[service] creating empty env file {}", launch.env_file.display());
        fs::write(launch.env_file, "").ok();
    }

    // Management API bearer token. Minted on first call, persisted to
    // `~/.okto/lair/.mgmt-token` (chmod 0600), and supplied to lair via
    // `-e LAIR_MGMT_TOKEN=...`. Children never see it — lair strips the env
    // var before spawning them, and they run as a different uid so
    // `/proc/1/environ` is also inaccessible.
    let mgmt_token = ensure_mgmt_token()?;

    // Custom seccomp profile so rootless container builders inside lair
    // (buildah running as non-root agent uids) can call
    // `unshare(CLONE_NEWUSER)`. Docker's default profile gates that syscall
    // behind CAP_SYS_ADMIN, which agents don't have — making every rootless
    // image build fail with "Operation not permitted" before buildah's flag
    // parsing runs. The bundled profile is Docker's default with the
    // namespace-creation syscalls allowed unconditionally; everything else
    // stays filtered.
    let seccomp_path = ensure_seccomp_profile()?;
    let seccomp_arg = format!("seccomp={}", seccomp_path.display());

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
        "--name",          LAIR_CONTAINER_NAME,
        "--restart",       "unless-stopped",
        "--security-opt",  &seccomp_arg,
        "-p",              &publish_noise,
        "-p",              &publish_http,
        "-v",              &mount,
        "--env-file",      &env_file_arg,
        // Tell lair which port to advertise in the QR code. NOISE_PORT inside
        // the container is hardcoded to 8443 (the EXPOSE'd port); PUBLIC_PORT
        // is what the mobile client should connect to from outside.
        "-e",              &public_port,
        // Token for the CLI ↔ lair management API. Set last so we can be
        // sure it overrides any LAIR_MGMT_TOKEN that snuck into the env
        // file (the parser of which doesn't reject it as a managed key).
        "-e",              &mgmt_env,
        launch.image,
    ];

    info!(
        "[service] starting lair container '{LAIR_CONTAINER_NAME}' (image={}, noise_port={}, http_port={})",
        launch.image, launch.noise_port, launch.http_port,
    );
    let out = docker_output(&args)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        error!("[service] `docker run` failed: {}", stderr.trim());
        anyhow::bail!("docker run failed: {stderr}");
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    info!("[service] lair container started: {}", id.chars().take(12).collect::<String>());
    Ok(id)
}

/// Stop + remove the lair container if one exists. Idempotent.
pub fn stop_lair_if_running() {
    if !container_exists() { return; }
    debug!("[service] removing existing lair container '{LAIR_CONTAINER_NAME}'");
    match docker_status(&["rm", "-f", LAIR_CONTAINER_NAME]) {
        Ok(s) if s.success() => info!("[service] lair container '{LAIR_CONTAINER_NAME}' removed"),
        Ok(s)  => warn!("[service] `docker rm -f {LAIR_CONTAINER_NAME}` exited with {s}"),
        Err(e) => warn!("[service] failed to remove lair container: {e:#}"),
    }
}

/// `docker pull <image>`. Used by `okto lair update`.
pub fn docker_pull(image: &str) -> Result<()> {
    ensure_docker_present()?;
    debug!("[service] shelling out: docker pull {image}");
    let status = std::process::Command::new("docker")
        .args(["pull", image])
        .status()
        .with_context(|| format!("spawn `docker pull {image}`"))?;
    debug!("[service] `docker pull {image}` exited with {status}");
    if !status.success() {
        error!("[service] `docker pull {image}` failed with {status}");
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
    debug!("[service] polling lair health endpoint GET {url} (timeout {timeout:?})");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::Instant::now() > deadline {
            error!("[service] lair did not become ready within {timeout:?} (GET {url})");
            anyhow::bail!("lair did not become ready within {:?}", timeout);
        }
        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                debug!("[service] lair health check GET {url} -> {}", r.status());
                return Ok(());
            }
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

pub async fn detect_public_ip() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    debug!("[service] detecting public IP via GET https://api.ipify.org");
    let resp = client.get("https://api.ipify.org").send().await
        .context("detect public IP via api.ipify.org")?;
    debug!("[service] api.ipify.org responded {}", resp.status());
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
    debug!("[service] shelling out: docker logs --tail 1000{} {LAIR_CONTAINER_NAME}", if follow { " --follow" } else { "" });
    let status = cmd.status().context("spawn `docker logs`")?;
    debug!("[service] `docker logs {LAIR_CONTAINER_NAME}` exited with {status}");
    if !status.success() {
        error!("[service] `docker logs {LAIR_CONTAINER_NAME}` exited with {status}");
        anyhow::bail!("`docker logs {LAIR_CONTAINER_NAME}` exited with {status}");
    }
    Ok(())
}
