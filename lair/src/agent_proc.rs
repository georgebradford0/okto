//! Process supervisor for child agent processes.
//!
//! Replaces what `lair/src/docker.rs` did against the Docker daemon. Each
//! agent is a separate `lair --role agent` OS process spawned by lair
//! with a per-agent data dir, workspace dir, and HTTP port (loopback only).
//! Lair proxies mobile WebSocket traffic to the child's local HTTP port.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use okto_core::mcp::McpServerConfig;
use tokio::process::{Child, Command};
use tracing::{debug, error, info, warn};

/// Legacy non-root uid/gid baked into the lair image as the `okto-agent`
/// user (see `lair/Dockerfile`). Used as a fallback when the supervisor
/// can't allocate a per-agent uid slot from the 10100..10199 range — e.g.
/// when the child's port falls outside the standard 30100..30199 range.
const FALLBACK_AGENT_UID: u32 = 10001;
const FALLBACK_AGENT_GID: u32 = 10001;

/// Per-agent uid range. Maps 1:1 to the agent port range (30100..30199):
/// port 30100 → uid 10100, ..., port 30199 → uid 10199. Each child runs as
/// its own uid so siblings can't read each other's `OKTO_AGENT_TOKEN` via
/// `/proc/<pid>/environ`.
const PORT_RANGE_BASE: u16 = 30100;
const PORT_RANGE_LEN:  u16 = 100;
const UID_RANGE_BASE:  u32 = 10100;

/// Resolve the (uid, gid) a child agent should run as, given its port.
/// Falls back to `(FALLBACK_AGENT_UID, FALLBACK_AGENT_GID)` when the port
/// is outside the standard range.
fn uid_for_port(port: u16) -> (u32, u32) {
    if port >= PORT_RANGE_BASE && port < PORT_RANGE_BASE + PORT_RANGE_LEN {
        let offset = (port - PORT_RANGE_BASE) as u32;
        let uid = UID_RANGE_BASE + offset;
        (uid, uid)
    } else {
        (FALLBACK_AGENT_UID, FALLBACK_AGENT_GID)
    }
}

/// Best-effort `chown(uid, gid)` — logs and continues on failure. Used
/// when the spawning lair process is non-root (e.g. dev mode) and the
/// chown would EPERM; in that case lair and the child are already the
/// same uid so the file permissions don't matter.
fn chown_best_effort(path: &Path, uid: u32, gid: u32) {
    if let Err(e) = std::os::unix::fs::chown(path, Some(uid), Some(gid)) {
        warn!("[supervisor] chown {} -> {uid}:{gid} failed: {e} (continuing)", path.display());
    }
}

/// Resolve the system username for a uid baked into the lair image. Mirrors
/// the `useradd` block in `lair/Dockerfile`:
///   * uid 10001            → `okto-agent`             (legacy fallback)
///   * uid 10100..=10199    → `okto-agent-<uid-10100>` (per-port slot)
/// Returns `None` for uids outside those ranges (dev-mode runs where the
/// image's users don't exist).
fn username_for_uid(uid: u32) -> Option<String> {
    if uid == FALLBACK_AGENT_UID {
        return Some("okto-agent".to_string());
    }
    if (UID_RANGE_BASE..UID_RANGE_BASE + PORT_RANGE_LEN as u32).contains(&uid) {
        return Some(format!("okto-agent-{}", uid - UID_RANGE_BASE));
    }
    None
}

/// Best-effort rewrite of a user's `pw_dir` field in `/etc/passwd` so
/// OpenSSH (and anything else that looks up `~` via `getpwuid()` rather
/// than `$HOME`) finds the per-agent `.ssh/` we just seeded. Done by
/// direct file edit rather than `usermod` because `usermod` refuses to
/// modify a user that has any running processes — which is fatal for the
/// lair case, where root is PID 1. Atomic via temp file + rename.
///
/// Failures (no `/etc/passwd`, user line missing, EPERM in dev mode) are
/// logged and ignored — the caller is already running as the target uid
/// in those cases, so the file ownership the chowns set up is sufficient
/// and ssh's default lookup is moot.
pub(crate) fn set_passwd_home_best_effort(user: &str, home: &Path, log_tag: &str) {
    let home_str = home.to_string_lossy();
    let passwd_path = std::path::Path::new("/etc/passwd");
    let original = match std::fs::read_to_string(passwd_path) {
        Ok(s) => s,
        Err(e) => {
            debug!("[{log_tag}] read /etc/passwd failed: {e} (continuing)");
            return;
        }
    };
    let prefix = format!("{user}:");
    let mut found = false;
    let mut changed = false;
    let mut rebuilt = String::with_capacity(original.len() + 16);
    for line in original.lines() {
        if !line.starts_with(&prefix) {
            rebuilt.push_str(line);
            rebuilt.push('\n');
            continue;
        }
        found = true;
        let mut fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 7 {
            rebuilt.push_str(line);
            rebuilt.push('\n');
            continue;
        }
        if fields[5] == home_str {
            rebuilt.push_str(line);
            rebuilt.push('\n');
            continue;
        }
        fields[5] = &home_str;
        rebuilt.push_str(&fields.join(":"));
        rebuilt.push('\n');
        changed = true;
    }
    if !found {
        debug!("[{log_tag}] /etc/passwd has no row for '{user}' (continuing)");
        return;
    }
    if !changed {
        debug!("[{log_tag}] /etc/passwd already has home={home_str} for {user}");
        return;
    }
    let tmp_path = passwd_path.with_extension("okto-tmp");
    if let Err(e) = std::fs::write(&tmp_path, &rebuilt) {
        warn!("[{log_tag}] write {} failed: {e} (continuing)", tmp_path.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp_path, passwd_path) {
        warn!(
            "[{log_tag}] rename {} -> /etc/passwd failed: {e} (continuing)",
            tmp_path.display(),
        );
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }
    debug!("[{log_tag}] /etc/passwd: set home={home_str} for {user}");
}

/// Seed `<agent_dir>/.ssh/` with the container-level SSH keypair (which
/// lair generated on startup at `$HOME/.ssh/id_ed25519{,.pub}`). All
/// agents in the same container share this one identity — the operator
/// only registers one pubkey per container on external services. Best-
/// effort: if lair's own keypair is missing (startup keygen failed), the
/// agent boots without `~/.ssh/` populated and SSH-from-agent won't work
/// until lair is restarted.
fn seed_agent_ssh(agent_dir: &Path, uid: u32, gid: u32) -> Result<()> {
    let lair_home = std::env::var("HOME")
        .ok().filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set — cannot resolve container ssh key"))?;
    let src_priv = okto_core::container_ssh_private_key(&lair_home);
    let src_pub  = okto_core::container_ssh_public_key(&lair_home);
    if !src_priv.exists() || !src_pub.exists() {
        warn!(
            "[supervisor] container ssh key missing ({} / {}); skipping seed for {}",
            src_priv.display(), src_pub.display(), agent_dir.display(),
        );
        return Ok(());
    }

    let dst_dir  = agent_dir.join(".ssh");
    let dst_priv = dst_dir.join(okto_core::SSH_PRIVATE_KEY_FILE);
    let dst_pub  = dst_dir.join(okto_core::SSH_PUBLIC_KEY_FILE);
    std::fs::create_dir_all(&dst_dir)
        .with_context(|| format!("create {}", dst_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dst_dir)?.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&dst_dir, perms).ok();
    }

    std::fs::copy(&src_priv, &dst_priv)
        .with_context(|| format!("copy {} -> {}", src_priv.display(), dst_priv.display()))?;
    std::fs::copy(&src_pub, &dst_pub)
        .with_context(|| format!("copy {} -> {}", src_pub.display(), dst_pub.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dst_priv)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&dst_priv, perms).ok();
    }
    chown_best_effort(&dst_dir,  uid, gid);
    chown_best_effort(&dst_priv, uid, gid);
    chown_best_effort(&dst_pub,  uid, gid);
    debug!("[supervisor] seeded {} from container ssh keypair", dst_dir.display());
    Ok(())
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
    pub startup_prompt:    Option<&'a str>,
    pub anthropic_api_key: Option<&'a str>,
    pub openai_api_key:    Option<&'a str>,
    pub openai_api_url:    Option<&'a str>,
    pub model:             Option<&'a str>,
    pub gh_token:          Option<&'a str>,
    pub agent_purpose:     Option<&'a str>,
    /// Capability token (random base64) the child uses to authenticate calls
    /// back to lair's agent-scoped management endpoints (e.g. spawn a
    /// grandchild, terminate one of its own descendants). Passed via
    /// `OKTO_AGENT_TOKEN` env. `None` for operator-spawned agents that
    /// don't get spawn/terminate capability (children of children get one
    /// from the spawning flow).
    pub agent_token:       Option<&'a str>,
    /// URL the child should hit to reach lair's loopback management API
    /// (e.g. `http://127.0.0.1:8000`). Passed via `LAIR_INTERNAL_URL`.
    pub lair_internal_url: Option<&'a str>,
    /// MCP servers to seed into the child's `mcp.json` before the child
    /// process starts. `None` means "leave the existing file alone" — used
    /// on restart so per-agent edits via `okto mcp add --agent <name>`
    /// survive. `Some(list)` writes that list to
    /// `<data_dir>/mcp.json`, overwriting any existing content. Pass an
    /// empty slice to explicitly clear the child's MCP servers.
    pub mcp:               Option<&'a [McpServerConfig]>,
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
    /// Path to the `lair` binary. Resolved at supervisor creation:
    /// `$OKTO_LAIR_BINARY` env override, or the current binary's path.
    binary_path:  PathBuf,
}

impl AgentSupervisor {
    pub fn new(agents_root: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&agents_root)
            .with_context(|| format!("create agents root {}", agents_root.display()))?;
        let binary_path = match std::env::var("OKTO_LAIR_BINARY") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => std::env::current_exe().context("locate current lair binary")?,
        };
        info!(
            "[agent_proc] supervisor ready: agents_root={} binary={}",
            agents_root.display(), binary_path.display(),
        );
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
        info!(
            "[agent_proc] spawning agent='{}' port={} git={} token={}",
            p.name,
            p.port,
            p.git_url.unwrap_or("(none)"),
            if p.agent_token.is_some() { "yes" } else { "no" },
        );
        let agent_dir     = self.agent_dir(p.name);
        let data_dir      = self.data_dir(p.name);
        let workspace_dir = self.workspace_dir(p.name);
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("create {}", data_dir.display()))?;
        std::fs::create_dir_all(&workspace_dir)
            .with_context(|| format!("create {}", workspace_dir.display()))?;
        std::fs::create_dir_all(&agent_dir)
            .with_context(|| format!("create {}", agent_dir.display()))?;

        // Per-agent uid baked into the image (see `lair/Dockerfile`). Each
        // child runs as its own uid so siblings can't read each other's
        // `OKTO_AGENT_TOKEN` from `/proc/<pid>/environ`.
        let (uid, gid) = uid_for_port(p.port);
        debug!("[agent_proc] agent='{}' will run as uid={uid} gid={gid}", p.name);

        // Drop the new dirs (and the existing log file) to the agent uid so
        // the child can write to them despite running non-root. Best-effort:
        // if the lair binary itself is running non-root (dev mode, weird
        // host setup) the chowns just fail with EPERM and we proceed —
        // children would already be running as the same uid in that case.
        chown_best_effort(&agent_dir,     uid, gid);
        chown_best_effort(&data_dir,      uid, gid);
        chown_best_effort(&workspace_dir, uid, gid);

        // Seed the agent's `~/.ssh/` with the container's shared SSH
        // keypair so plain `ssh user@host` from the agent's bash tool
        // works without `-i` flags. All agents in the lair container share
        // one identity — the operator registers one pubkey per container
        // on external services (Prime Intellect, GitHub, etc.).
        if let Err(e) = seed_agent_ssh(&agent_dir, uid, gid) {
            warn!("[supervisor] seed_agent_ssh for '{}': {e:#} (agent will boot without ~/.ssh/)", p.name);
        }

        // Point the agent uid's `pw_dir` at its per-agent dir. OpenSSH
        // resolves `~` via `getpwuid()`, not `$HOME`, so without this
        // `ssh user@host` from the child's bash would look in the image-
        // default `/home/okto-agent[-N]/.ssh/` and miss the key we just
        // seeded. Same uid is reused across agent names over time, so
        // this runs on every spawn rather than being baked in at image
        // build time.
        if let Some(user) = username_for_uid(uid) {
            set_passwd_home_best_effort(&user, &agent_dir, "supervisor");
        }

        // Seed the child's mcp.json *before* spawning so the child's
        // `init_mcp_pool()` sees it on first read. `None` means the caller
        // (typically a restart path) wants to preserve whatever's already
        // on disk — possibly per-agent edits made via
        // `okto mcp add --agent <name>` after the child was created.
        if let Some(servers) = p.mcp {
            let mcp_path = data_dir.join("mcp.json");
            let json = serde_json::to_string_pretty(servers)
                .context("serialize seeded mcp.json")?;
            std::fs::write(&mcp_path, json)
                .with_context(|| format!("write {}", mcp_path.display()))?;
            chown_best_effort(&mcp_path, uid, gid);
            info!("[supervisor] seeded {} with {} MCP server(s)", mcp_path.display(), servers.len());
        }

        let log_path = self.log_path(p.name);
        let log_file = std::fs::OpenOptions::new()
            .create(true).append(true).open(&log_path)
            .with_context(|| format!("open log file at {}", log_path.display()))?;
        chown_best_effort(&log_path, uid, gid);
        let log_file2 = log_file.try_clone().context("clone log fd for stderr")?;

        let mut cmd = Command::new(&self.binary_path);
        cmd.arg("--role").arg("agent");
        cmd.env("OKTO_DATA_DIR",   &data_dir);
        cmd.env("WORKSPACE_DIR",   &workspace_dir);
        cmd.env("AGENT_PORT",      p.port.to_string());
        cmd.env("AGENT_NAME",      p.name);
        // Children share the operator's config.json via core::config_dir();
        // no need to bake credentials into env. Skip the login-shell env
        // bootstrap (we already have what we need from the parent's env).
        cmd.env("OKTO_SKIP_SHELL_ENV", "1");
        // Mark this as a locally-spawned child so the agent role skips the
        // container bootstrap script — lair already ran it for the shared
        // container (see `bootstrap::run_bootstrap_script`).
        cmd.env("OKTO_LOCAL_CHILD", "1");
        // The child runs as a non-root uid; give it a writable HOME so
        // npm/uvx/gh/git caches land somewhere it can actually write.
        cmd.env("HOME", &agent_dir);
        if std::env::var("OKTO_DEV").as_deref() == Ok("1") {
            cmd.env("OKTO_DEV", "1");
        }
        if let Some(v) = p.git_url        { cmd.env("GIT_URL",         v); }
        if let Some(v) = p.startup_prompt { cmd.env("STARTUP_PROMPT",  v); }
        if let Some(v) = p.agent_purpose  { cmd.env("AGENT_PURPOSE",   v); }
        // Forward provider creds via env so the child doesn't need to read
        // the global config.json. ANTHROPIC_API_KEY / OPENAI_API_KEY /
        // OPENAI_API_URL / MODEL match the precedence rules in
        // `okto_core::resolve_api_key` (env > config).
        if let Some(v) = p.anthropic_api_key { cmd.env("ANTHROPIC_API_KEY", v); }
        if let Some(v) = p.openai_api_key    { cmd.env("OPENAI_API_KEY",    v); }
        if let Some(v) = p.openai_api_url    { cmd.env("OPENAI_API_URL",    v); }
        if let Some(v) = p.model             { cmd.env("MODEL",             v); }
        if let Some(v) = p.gh_token          { cmd.env("GH_TOKEN",          v); }
        if let Some(v) = p.agent_token       { cmd.env("OKTO_AGENT_TOKEN",  v); }
        if let Some(v) = p.lair_internal_url { cmd.env("LAIR_INTERNAL_URL", v); }

        // Never let the child inherit lair's management-API token or any
        // other lair-private knobs. With the uid drop below, `/proc/1/environ`
        // is also unreadable from the child's uid, so this is belt+suspenders.
        cmd.env_remove("LAIR_MGMT_TOKEN");

        cmd.stdin(Stdio::null())
           .stdout(Stdio::from(log_file))
           .stderr(Stdio::from(log_file2))
           .kill_on_drop(false);

        // Drop privileges to the per-agent uid baked into the image.
        // This prevents a child from `kill 1`-ing lair (lair runs as root
        // inside the container), reading root-only files like
        // `/proc/1/environ` and `/data/config.json`, and reading any
        // sibling agent's `OKTO_AGENT_TOKEN` from its `/proc/<pid>/environ`.
        cmd.uid(uid).gid(gid);

        let child = cmd.spawn()
            .map_err(|e| {
                error!("[agent_proc] failed to spawn agent process for '{}': {e}", p.name);
                e
            })
            .with_context(|| format!("spawn agent process for {}", p.name))?;
        let pid = child.id().ok_or_else(|| {
            error!("[agent_proc] spawned child for '{}' has no pid", p.name);
            anyhow::anyhow!("spawned child has no pid")
        })?;
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
        debug!("[agent_proc] stop({name}): sending SIGTERM to pid={pid}");
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
        for _ in 0..30 {
            if !Self::is_alive(pid) { break; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if Self::is_alive(pid) {
            warn!("[agent_proc] stop({name}): pid={pid} ignored SIGTERM after 3s grace, escalating to SIGKILL");
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
            } else {
                info!("[supervisor] terminated agent='{name}' and removed {}", dir.display());
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
        if !Self::is_alive(pid) {
            debug!("[supervisor] adopt({name}): recorded pid={pid} is no longer alive, skipping");
            return;
        }
        let proc = Arc::new(AgentProc {
            pid,
            child: Mutex::new(None), // we don't own the handle; can't reap
        });
        self.agents.lock().unwrap().insert(name.to_string(), proc);
        info!("[supervisor] adopted existing agent='{name}' pid={pid}");
    }

    /// Tail the last `bytes` of an agent's log file. Used by the CLI's
    /// `okto logs <agent>` command. Returns the whole file if smaller.
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

