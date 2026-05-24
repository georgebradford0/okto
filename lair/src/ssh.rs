//! Outbound SSH from lair to a remote-VM agent. Used by
//! `register_remote_agent` to (a) pull the agent's published identity from
//! `/var/lib/okto/lair/agent-info.json` (host path; bind-mounted to
//! `/data/lair/agent-info.json` inside the agent container) after cloud-init
//! finishes, and (b) drop the operator's `config.json` + optionally clone a
//! git repo, then `systemctl restart okto-agent` (which restarts the
//! `docker run`'d container).
//!
//! Shells out to the system `ssh` binary so host-key handling, key auth, and
//! `known_hosts` come for free.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, error, info, warn};

/// Where the agent process publishes its identity on the remote VM. The
/// agent container has `OKTO_DATA_DIR=/data/lair` baked in by the image and
/// runs with `-v /var/lib/okto:/data`, so the host-side path is
/// `/var/lib/okto/lair/agent-info.json`. Lair reads it over SSH.
pub const REMOTE_AGENT_INFO_PATH: &str = "/var/lib/okto/lair/agent-info.json";

/// Where lair drops the operator's `config.json` on the remote VM. The
/// agent container has `OKTO_HOME=/data` baked in, so it reads
/// `/data/config.json` — host path with the bind mount is
/// `/var/lib/okto/config.json`.
pub const REMOTE_CONFIG_PATH:     &str = "/var/lib/okto/config.json";

/// Workspace dir on the remote VM. The userdata sets the container's
/// `WORKSPACE_DIR=/data/workspace`; host-side it's `/var/lib/okto/workspace`.
pub const REMOTE_WORKSPACE_PATH:  &str = "/var/lib/okto/workspace";

/// Lair-side known_hosts file. Stored under `OKTO_DATA_DIR` so accept-new
/// entries persist across lair restarts.
pub fn known_hosts_path() -> PathBuf {
    okto_core::data_dir().join("known_hosts")
}

/// Parsed contents of `/var/lib/okto/lair/agent-info.json` as written by
/// the agent role inside its container (the container writes
/// `/data/lair/agent-info.json` which the host sees at
/// `/var/lib/okto/lair/agent-info.json` via the bind mount).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AgentInfo {
    pub pubkey:   String,
    pub port:     u16,
    #[serde(default)]
    pub ready_at: u64,
}

async fn try_read_once(
    host:          &str,
    ssh_user:      &str,
    key_path:      &Path,
    connect_secs:  u64,
) -> Result<Option<AgentInfo>> {
    let known_hosts = known_hosts_path();
    if let Some(parent) = known_hosts.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let target = format!("{ssh_user}@{host}");
    let connect = format!("ConnectTimeout={connect_secs}");
    let known   = format!("UserKnownHostsFile={}", known_hosts.display());
    let key     = key_path.to_string_lossy().to_string();
    let remote_cat = format!("cat {REMOTE_AGENT_INFO_PATH} 2>/dev/null");

    let output = Command::new("ssh")
        .args([
            "-i",                              key.as_str(),
            "-o", "StrictHostKeyChecking=accept-new",
            "-o", "BatchMode=yes",
            "-o", &connect,
            "-o", &known,
            target.as_str(),
            remote_cat.as_str(),
        ])
        .output()
        .await
        .context("spawn ssh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if let Some(code) = output.status.code() {
            if code == 255 {
                error!("[ssh] {host}: ssh transport failed (exit 255): {}", stderr.trim());
                anyhow::bail!("ssh to {host} failed: {}", stderr.trim());
            }
        }
        debug!("[ssh] {host} agent-info not yet present");
        return Ok(None);
    }

    let stdout = std::str::from_utf8(&output.stdout)
        .context("ssh stdout is not utf-8")?
        .trim();
    if stdout.is_empty() {
        debug!("[ssh] {host} agent-info.json exists but is empty");
        return Ok(None);
    }
    let info: AgentInfo = serde_json::from_str(stdout)
        .with_context(|| format!("parse agent-info.json from {host}"))?;
    debug!("[ssh] {host} published agent-info: port={}", info.port);
    Ok(Some(info))
}

/// Poll the remote VM via SSH until it publishes `agent-info.json` or the
/// total timeout elapses. Cloud-init on a fresh VM commonly takes 1–3 min.
pub async fn await_agent_info(
    host:          &str,
    ssh_user:      &str,
    key_path:      &Path,
    total_timeout: Duration,
    poll_every:    Duration,
) -> Result<AgentInfo> {
    info!("[ssh] {host}: polling for agent-info.json (timeout {total_timeout:?}, every {poll_every:?})");
    let deadline = tokio::time::Instant::now() + total_timeout;
    let mut last_err: Option<anyhow::Error> = None;
    while tokio::time::Instant::now() < deadline {
        match try_read_once(host, ssh_user, key_path, /*connect_secs=*/10).await {
            Ok(Some(info)) => {
                info!("[ssh] {host}: agent-info.json retrieved (port={})", info.port);
                return Ok(info);
            }
            Ok(None) => {
                last_err = None;
                tokio::time::sleep(poll_every).await;
            }
            Err(e) => {
                warn!("[ssh] {host}: {e:#}; retrying");
                last_err = Some(e);
                tokio::time::sleep(poll_every).await;
            }
        }
    }
    error!("[ssh] {host}: timed out waiting for agent-info.json after {total_timeout:?}");
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!(
        "timed out waiting for {host} to publish agent-info.json after {:?}",
        total_timeout
    )))
}

pub fn read_lair_public_key() -> Result<String> {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set — cannot resolve lair container SSH pubkey path")?;
    let path = okto_core::container_ssh_public_key(&home);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read lair ssh public key at {}", path.display()))?;
    Ok(text.trim().to_string())
}

fn ssh_argv(key_path: &Path, target: &str) -> Vec<String> {
    let known_hosts = known_hosts_path();
    vec![
        "-i".to_string(),  key_path.to_string_lossy().into_owned(),
        "-o".to_string(),  "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),  "BatchMode=yes".to_string(),
        "-o".to_string(),  "ConnectTimeout=10".to_string(),
        "-o".to_string(),  format!("UserKnownHostsFile={}", known_hosts.display()),
        target.to_string(),
    ]
}

const SSH_OP_ATTEMPTS:      u32      = 4;
const SSH_OP_INITIAL_DELAY: Duration = Duration::from_secs(2);

async fn retry_ssh_op<F, Fut, T>(label: &str, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay = SSH_OP_INITIAL_DELAY;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=SSH_OP_ATTEMPTS {
        match op().await {
            Ok(v) => {
                if attempt > 1 {
                    info!("[ssh] {label} succeeded on attempt {attempt}/{SSH_OP_ATTEMPTS}");
                }
                return Ok(v);
            }
            Err(e) => {
                if attempt < SSH_OP_ATTEMPTS {
                    warn!("[ssh] {label} attempt {attempt}/{SSH_OP_ATTEMPTS} failed ({e:#}); retrying in {:?}", delay);
                    last_err = Some(e);
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                } else {
                    error!("[ssh] {label} failed after {SSH_OP_ATTEMPTS} attempts: {e:#}");
                    return Err(e.context(format!("{label} failed after {SSH_OP_ATTEMPTS} attempts")));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("{label}: exhausted retries")))
}

pub async fn write_file(
    host:        &str,
    ssh_user:    &str,
    key_path:    &Path,
    remote_path: &str,
    content:     &str,
    mode:        u32,
) -> Result<()> {
    let label = format!("write_file {remote_path}@{host}");
    retry_ssh_op(&label, || async {
        write_file_once(host, ssh_user, key_path, remote_path, content, mode).await
    }).await
}

async fn write_file_once(
    host:        &str,
    ssh_user:    &str,
    key_path:    &Path,
    remote_path: &str,
    content:     &str,
    mode:        u32,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let target = format!("{ssh_user}@{host}");
    let remote_cmd = format!(
        "set -e; umask 0077; mkdir -p \"$(dirname {p})\"; cat > {p}; chmod {m:o} {p}",
        p = shell_escape(remote_path),
        m = mode,
    );

    let mut argv = ssh_argv(key_path, &target);
    argv.push(remote_cmd);

    debug!("[ssh] {host}: writing {} byte(s) to {remote_path} (mode {mode:o})", content.len());
    let mut child = tokio::process::Command::new("ssh")
        .args(&argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn ssh write_file")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(content.as_bytes()).await
            .context("write content to ssh stdin")?;
        stdin.shutdown().await.ok();
    }
    let output = child.wait_with_output().await.context("wait ssh write_file")?;
    if !output.status.success() {
        anyhow::bail!(
            "ssh write_file {remote_path} on {host}: {}",
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    debug!("[ssh] {host}: wrote {remote_path}");
    Ok(())
}

pub async fn run_script(
    host:     &str,
    ssh_user: &str,
    key_path: &Path,
    script:   &str,
) -> Result<String> {
    let label = format!("run_script@{host}");
    retry_ssh_op(&label, || async {
        run_script_once(host, ssh_user, key_path, script).await
    }).await
}

async fn run_script_once(
    host:     &str,
    ssh_user: &str,
    key_path: &Path,
    script:   &str,
) -> Result<String> {
    use tokio::io::AsyncWriteExt;
    let target = format!("{ssh_user}@{host}");
    let mut argv = ssh_argv(key_path, &target);
    argv.push("bash -s".to_string());

    debug!("[ssh] {host}: running remote script ({} chars)", script.len());
    let mut child = tokio::process::Command::new("ssh")
        .args(&argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn ssh run_script")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes()).await
            .context("write script to ssh stdin")?;
        stdin.shutdown().await.ok();
    }
    let output = child.wait_with_output().await.context("wait ssh run_script")?;
    if !output.status.success() {
        anyhow::bail!(
            "ssh run_script on {host}: {}",
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    debug!("[ssh] {host}: remote script completed");
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
