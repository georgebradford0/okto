//! Process supervisor for child agent processes.
//!
//! Replaces what `lair/src/docker.rs` did against the Docker daemon. Each
//! agent is a separate `octo-lair --role agent` OS process spawned by lair
//! with a per-agent data dir, workspace dir, and HTTP port (loopback only).
//! Lair proxies mobile WebSocket traffic to the child's local HTTP port.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use tracing::{info, warn};

/// Non-root uid/gid baked into the lair image as the `octo-agent` user
/// (see `lair/Dockerfile`). Child agent processes drop to this uid before
/// exec'ing, which prevents them from `kill 1`-ing lair, reading
/// `/proc/1/environ`, or reading root-owned files under `/data`.
const AGENT_UID: u32 = 10001;
const AGENT_GID: u32 = 10001;

/// Best-effort `chown(uid, gid)` — logs and continues on failure. Used
/// when the spawning lair process is non-root (e.g. dev mode) and the
/// chown would EPERM; in that case lair and the child are already the
/// same uid so the file permissions don't matter.
fn chown_best_effort(path: &Path, uid: u32, gid: u32) {
    if let Err(e) = std::os::unix::fs::chown(path, Some(uid), Some(gid)) {
        warn!("[supervisor] chown {} -> {uid}:{gid} failed: {e} (continuing)", path.display());
    }
}

/// Per-agent spawn params handed to `AgentSupervisor::spawn`. Mirrors the
/// shape of the old `CreateAgentParams` so call sites in `lair.rs` stay
/// readable.
#[derive(Clone, Debug)]
pub struct SpawnParams<'a> {
    pub name:              &'a str,
    /// Loopback HTTP port the child binds. Allocated from 30100–30199 by the
    /// registry. Lair proxies WS traffic to this port.
    pub port:              u16,
    pub git_url:           Option<&'a str>,
    pub startup_script:    Option<&'a str>,
    pub startup_prompt:    Option<&'a str>,
    pub anthropic_api_key: Option<&'a str>,
    pub openai_api_key:    Option<&'a str>,
    pub openai_api_url:    Option<&'a str>,
    pub model:             Option<&'a str>,
    pub gh_token:          Option<&'a str>,
    pub agent_purpose:     Option<&'a str>,
}

/// One running agent. We keep the `Child` handle so dropping the supervisor
/// kills the child (Tokio's default behaviour); explicit shutdown uses
/// `Child::kill` + `wait` for a graceful exit.
struct AgentProc {
    pid:    u32,
    child:  Mutex<Option<Child>>,
}

/// Tracks every child agent process lair has spawned. Holds the `Child`
/// handles so the OS doesn't leak zombies on lair shutdown, and exposes
/// helpers for spawning, stopping, and checking liveness.
pub struct AgentSupervisor {
    agents:       Mutex<HashMap<String, Arc<AgentProc>>>,
    agents_root:  PathBuf,
    /// Path to the `octo-lair` binary. Resolved at supervisor creation:
    /// `$OCTO_LAIR_BINARY` env override, or the current binary's path.
    binary_path:  PathBuf,
}

impl AgentSupervisor {
    pub fn new(agents_root: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&agents_root)
            .with_context(|| format!("create agents root {}", agents_root.display()))?;
        let binary_path = match std::env::var("OCTO_LAIR_BINARY") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => std::env::current_exe().context("locate current octo-lair binary")?,
        };
        Ok(Arc::new(Self {
            agents:      Mutex::new(HashMap::new()),
            agents_root,
            binary_path,
        }))
    }

    pub fn agent_dir(&self, name: &str) -> PathBuf  { self.agents_root.join(name) }
    pub fn data_dir(&self,  name: &str) -> PathBuf  { self.agent_dir(name).join("data") }
    pub fn workspace_dir(&self, name: &str) -> PathBuf { self.agent_dir(name).join("workspace") }
    pub fn log_path(&self, name: &str) -> PathBuf   { self.agent_dir(name).join("agent.log") }

    /// Spawn a child agent process. Caller is responsible for inserting the
    /// matching `AgentRecord` into the registry — the supervisor doesn't
    /// touch the registry directly.
    pub async fn spawn(&self, p: &SpawnParams<'_>) -> Result<u32> {
        let agent_dir     = self.agent_dir(p.name);
        let data_dir      = self.data_dir(p.name);
        let workspace_dir = self.workspace_dir(p.name);
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("create {}", data_dir.display()))?;
        std::fs::create_dir_all(&workspace_dir)
            .with_context(|| format!("create {}", workspace_dir.display()))?;
        std::fs::create_dir_all(&agent_dir)
            .with_context(|| format!("create {}", agent_dir.display()))?;

        // Drop the new dirs (and the existing log file) to the agent uid so
        // the child can write to them despite running non-root. Best-effort:
        // if the lair binary itself is running non-root (dev mode, weird
        // host setup) the chowns just fail with EPERM and we proceed —
        // children would already be running as the same uid in that case.
        chown_best_effort(&agent_dir,     AGENT_UID, AGENT_GID);
        chown_best_effort(&data_dir,      AGENT_UID, AGENT_GID);
        chown_best_effort(&workspace_dir, AGENT_UID, AGENT_GID);

        let log_path = self.log_path(p.name);
        let log_file = std::fs::OpenOptions::new()
            .create(true).append(true).open(&log_path)
            .with_context(|| format!("open log file at {}", log_path.display()))?;
        chown_best_effort(&log_path, AGENT_UID, AGENT_GID);
        let log_file2 = log_file.try_clone().context("clone log fd for stderr")?;

        let mut cmd = Command::new(&self.binary_path);
        cmd.arg("--role").arg("agent");
        cmd.env("OCTO_DATA_DIR",   &data_dir);
        cmd.env("WORKSPACE_DIR",   &workspace_dir);
        cmd.env("AGENT_PORT",      p.port.to_string());
        // Children share the operator's config.json via core::config_dir();
        // no need to bake credentials into env. Skip the login-shell env
        // bootstrap (we already have what we need from the parent's env).
        cmd.env("OCTO_SKIP_SHELL_ENV", "1");
        // The child runs as a non-root uid; give it a writable HOME so
        // npm/uvx/gh/git caches land somewhere it can actually write.
        cmd.env("HOME", &agent_dir);
        if std::env::var("OCTO_DEV").as_deref() == Ok("1") {
            cmd.env("OCTO_DEV", "1");
        }
        if let Some(v) = p.git_url        { cmd.env("GIT_URL",         v); }
        if let Some(v) = p.startup_script { cmd.env("STARTUP_SCRIPT",  v); }
        if let Some(v) = p.startup_prompt { cmd.env("STARTUP_PROMPT",  v); }
        if let Some(v) = p.agent_purpose  { cmd.env("AGENT_PURPOSE",   v); }
        // Forward provider creds via env so the child doesn't need to read
        // the global config.json. ANTHROPIC_API_KEY / OPENAI_API_KEY /
        // OPENAI_API_URL / MODEL match the precedence rules in
        // `octo_core::resolve_api_key` (env > config).
        if let Some(v) = p.anthropic_api_key { cmd.env("ANTHROPIC_API_KEY", v); }
        if let Some(v) = p.openai_api_key    { cmd.env("OPENAI_API_KEY",    v); }
        if let Some(v) = p.openai_api_url    { cmd.env("OPENAI_API_URL",    v); }
        if let Some(v) = p.model             { cmd.env("MODEL",             v); }
        if let Some(v) = p.gh_token          { cmd.env("GH_TOKEN",          v); }

        // Never let the child inherit lair's management-API token or any
        // other lair-private knobs. With the uid drop below, `/proc/1/environ`
        // is also unreadable from the child's uid, so this is belt+suspenders.
        cmd.env_remove("LAIR_MGMT_TOKEN");

        cmd.stdin(Stdio::null())
           .stdout(Stdio::from(log_file))
           .stderr(Stdio::from(log_file2))
           .kill_on_drop(false);

        // Drop privileges to the non-root agent user baked into the image.
        // This is the only thing that prevents a child from `kill 1`-ing
        // lair (lair runs as root inside the container) or reading
        // root-only files like `/proc/1/environ` and `/data/config.json`.
        cmd.uid(AGENT_UID).gid(AGENT_GID);

        let child = cmd.spawn()
            .with_context(|| format!("spawn agent process for {}", p.name))?;
        let pid = child.id().ok_or_else(|| anyhow::anyhow!("spawned child has no pid"))?;
        info!("[supervisor] spawned agent='{}' pid={} port={}", p.name, pid, p.port);

        let proc = Arc::new(AgentProc {
            pid,
            child: Mutex::new(Some(child)),
        });
        self.agents.lock().unwrap().insert(p.name.to_string(), proc);
        Ok(pid)
    }

    /// Stop a running agent. Sends SIGTERM, then SIGKILL after a short grace
    /// period. Idempotent.
    pub async fn stop(&self, name: &str) -> Result<()> {
        let (pid, mut owned_child) = {
            let mut map = self.agents.lock().unwrap();
            let Some(proc) = map.remove(name) else {
                info!("[supervisor] stop({name}): no in-memory handle");
                return Ok(());
            };
            let mut slot = proc.child.lock().unwrap();
            (proc.pid, slot.take())
        };

        // SIGTERM first.
        // SAFETY: standard libc::kill signal call.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
        for _ in 0..30 {
            if !Self::is_alive(pid) { break; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if Self::is_alive(pid) {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL); }
        }
        if let Some(child) = owned_child.as_mut() {
            let _ = child.wait().await;
        }
        info!("[supervisor] stopped agent='{name}' (pid={pid})");
        Ok(())
    }

    /// Stop + delete the per-agent data + workspace + logs.
    pub async fn terminate(&self, name: &str) -> Result<()> {
        self.stop(name).await?;
        let dir = self.agent_dir(name);
        if dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                warn!("[supervisor] remove_dir_all {}: {e}", dir.display());
            }
        }
        Ok(())
    }

    /// `kill(pid, 0)` style liveness check. Cheap; safe to call every poll.
    pub fn is_alive(pid: u32) -> bool {
        // `kill(pid, 0)` returns 0 if the process exists and we can signal it.
        // Linux-only: that's the whole supported surface now.
        // SAFETY: libc::kill with signal 0 is a pure liveness probe; no
        // signal is actually delivered.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// Adopt an already-running child agent that lair lost track of (e.g.
    /// across a lair restart). Inserts a placeholder `AgentProc` so future
    /// `stop` / `terminate` calls find a handle. The pid is checked with
    /// `is_alive` first; if it's dead, the entry is skipped.
    pub fn adopt(&self, name: &str, pid: u32) {
        if !Self::is_alive(pid) { return; }
        let proc = Arc::new(AgentProc {
            pid,
            child: Mutex::new(None), // we don't own the handle; can't reap
        });
        self.agents.lock().unwrap().insert(name.to_string(), proc);
        info!("[supervisor] adopted existing agent='{name}' pid={pid}");
    }

    /// Tail the last `bytes` of an agent's log file. Used by the CLI's
    /// `octo logs <agent>` command. Returns the whole file if smaller.
    pub fn log_tail(&self, name: &str, bytes: u64) -> Result<String> {
        let path = self.log_path(name);
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("stat {}", path.display()))?;
        let size = metadata.len();
        let offset = size.saturating_sub(bytes);
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        f.seek(SeekFrom::Start(offset)).ok();
        let mut buf = String::new();
        f.read_to_string(&mut buf).ok();
        Ok(buf)
    }
}

