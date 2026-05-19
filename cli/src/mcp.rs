//! `octo mcp …` — manage the per-process `mcp.json`.
//!
//! All configs live on the host filesystem now:
//!   - lair:  `~/.octo/lair/mcp.json`
//!   - agent: `~/.octo/agents/<name>/data/mcp.json`
//!
//! Both lair and child agent processes watch their `mcp.json` and hot-reload
//! on change. Adding a new entry is a plain file edit followed by tailing the
//! agent's log for the `[mcp] '<name>' connected` marker (lair's logs come
//! from `docker logs lair` since 0.7.0 — there is no on-disk lair.log).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::service;

pub const LAIR_AGENT_NAME: &str = "lair";

#[derive(Serialize, Deserialize, Clone, Debug)]
struct McpServerConfig {
    name:    String,
    #[serde(default)]
    command: String,
    #[serde(default)]
    args:    Vec<String>,
    #[serde(default)]
    env:     HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url:     Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    headers: HashMap<String, String>,
}

fn mcp_path(agent: &str) -> PathBuf {
    if agent == LAIR_AGENT_NAME {
        service::lair_data_dir().join("mcp.json")
    } else {
        service::agents_dir().join(agent).join("data").join("mcp.json")
    }
}

fn agent_log_path(agent: &str) -> PathBuf {
    // Caller must check `agent != LAIR_AGENT_NAME` first — lair has no
    // on-disk log file, only `docker logs lair`.
    service::agents_dir().join(agent).join("agent.log")
}

fn read_mcp(agent: &str) -> Result<Vec<McpServerConfig>> {
    let path = mcp_path(agent);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => t,
        _ => return Ok(Vec::new()),
    };
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn write_mcp(agent: &str, configs: &[McpServerConfig]) -> Result<()> {
    let path = mcp_path(agent);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_string_pretty(configs)?;
    debug!("[mcp] writing {} ({} server(s)) for agent '{agent}'", path.display(), configs.len());
    crate::init::write_secret_file(&path, &json)
}

// ── Log capture ──────────────────────────────────────────────────────────────

/// Read up to the last `bytes` of an agent's on-disk log file.
fn read_agent_log_tail(agent: &str, bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let path = agent_log_path(agent);
    let Ok(meta) = std::fs::metadata(&path) else { return String::new(); };
    let offset = meta.len().saturating_sub(bytes);
    let Ok(mut f) = std::fs::File::open(&path) else { return String::new(); };
    f.seek(SeekFrom::Start(offset)).ok();
    let mut buf = String::new();
    f.read_to_string(&mut buf).ok();
    buf
}

/// Read the entire current log buffer for `agent`. For lair this shells out
/// to `docker logs lair` (the container has no on-disk log file); for
/// child agents this reads the supervisor-written `agent.log`. Returns an
/// empty string on any error so callers can stay simple — they're scanning
/// for markers, not asserting log presence.
fn read_log_snapshot(agent: &str) -> String {
    if agent == LAIR_AGENT_NAME {
        service::read_lair_logs(5000).unwrap_or_default()
    } else {
        // 1 MiB is enough for ~10k log lines, which generously covers the
        // window of an MCP startup + handshake.
        read_agent_log_tail(agent, 1024 * 1024)
    }
}

/// Extract the leading RFC3339 timestamp from a structured-tracing log line.
/// Lines emitted by core/lair/agent tracing start with e.g.
/// `2026-05-15T04:16:24.632994684Z `. Returns the timestamp token, or `None`
/// for lines without one (blank lines, panic backtraces, etc).
fn line_timestamp(line: &str) -> Option<&str> {
    let ts = line.split_whitespace().next()?;
    let b = ts.as_bytes();
    if b.len() >= 20 && b[10] == b'T' && ts.ends_with('Z') {
        Some(ts)
    } else {
        None
    }
}

/// Timestamp of the most recent log line currently visible for `agent`, used
/// as the lower bound for post-write marker scanning. RFC3339 UTC timestamps
/// sort lexically, so a plain string compare orders them correctly. Returns
/// an empty string if no timestamped line is present (fresh container).
fn latest_log_timestamp(agent: &str) -> String {
    read_log_snapshot(agent)
        .lines()
        .rev()
        .find_map(line_timestamp)
        .unwrap_or_default()
        .to_string()
}

// ── Marker scanning ──────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpMarker {
    /// `[mcp] '<name>' connected` — server completed the handshake.
    Connected,
    /// `[mcp] warning: server '<name>' advertised no tools` — handshake OK
    /// but the server has nothing to offer (still considered a success).
    NoTools,
    /// `[mcp] failed to spawn '<name>': <reason>` — binary missing or
    /// non-executable.
    SpawnFailed(String),
    /// `[mcp] '<name>' initialize failed: <reason>` — process started but
    /// MCP handshake errored out.
    InitFailed(String),
    /// No marker observed within the timeout. Could mean the server is slow
    /// to start, the log watcher missed the event, or lair hot-reload hasn't
    /// noticed the file change yet.
    Timeout,
}

impl McpMarker {
    pub fn is_success(&self) -> bool {
        matches!(self, McpMarker::Connected | McpMarker::NoTools)
    }
}

/// Scan a log buffer for MCP markers tied to `name`. Returns the *first*
/// terminal marker (success or failure) encountered for that name, or
/// `Timeout` if none is present. Lines at or before `since` (an RFC3339
/// timestamp) are ignored so a previous lifetime's `connected` marker can't
/// be mistaken for a fresh one; an empty `since` scans everything.
fn classify_markers(name: &str, logs: &str, since: &str) -> McpMarker {
    let connected = format!("[mcp] '{name}' connected");
    let no_tools  = format!("[mcp] warning: server '{name}' advertised no tools");
    let spawn     = format!("[mcp] failed to spawn '{name}'");
    let init      = format!("[mcp] '{name}' initialize failed");

    for line in logs.lines() {
        if !since.is_empty() {
            // Markers are single-line tracing events and always carry a
            // timestamp; skip untimestamped lines and anything not strictly
            // newer than the pre-write cutoff.
            match line_timestamp(line) {
                Some(ts) if ts > since => {}
                _ => continue,
            }
        }
        if line.contains(&connected) { return McpMarker::Connected; }
        if line.contains(&no_tools)  { return McpMarker::NoTools; }
        if let Some(idx) = line.find(&spawn) {
            let reason = line[idx + spawn.len()..]
                .trim_start_matches(':').trim().to_string();
            return McpMarker::SpawnFailed(reason);
        }
        if let Some(idx) = line.find(&init) {
            let reason = line[idx + init.len()..]
                .trim_start_matches(':').trim().to_string();
            return McpMarker::InitFailed(reason);
        }
    }
    McpMarker::Timeout
}

pub struct WaitOpts<'a> {
    pub agent:    &'a str,
    pub names:    &'a [String],
    pub timeout:  Duration,
    /// RFC3339 timestamp of the last log line observed *before* the file
    /// write. Marker scanning ignores any line at or before this instant, so
    /// we don't pick up `connected` markers from a previous lifetime of the
    /// same server name. A byte offset can't be used here: lair's log source
    /// is `docker logs --tail`, a sliding window whose front scrolls away as
    /// the container keeps logging. Empty means "scan everything".
    pub since:    String,
}

/// Poll the relevant log source every 3 s until every `name` has a terminal
/// marker, or `timeout` elapses. Returns one entry per requested name.
pub async fn wait_for_mcp_markers(opts: WaitOpts<'_>) -> HashMap<String, McpMarker> {
    let deadline = tokio::time::Instant::now() + opts.timeout;
    let mut decided: HashMap<String, McpMarker> = HashMap::new();
    loop {
        let snapshot = read_log_snapshot(opts.agent);
        for name in opts.names {
            if decided.contains_key(name) { continue; }
            let marker = classify_markers(name, &snapshot, &opts.since);
            if marker != McpMarker::Timeout {
                decided.insert(name.clone(), marker);
            }
        }
        if decided.len() == opts.names.len() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    for name in opts.names {
        decided.entry(name.clone()).or_insert(McpMarker::Timeout);
    }
    decided
}

/// Format a per-server result block for printing.
pub fn format_marker_report(results: &HashMap<String, McpMarker>, names: &[String]) -> String {
    let mut out = String::new();
    for name in names {
        let m = results.get(name).cloned().unwrap_or(McpMarker::Timeout);
        match m {
            McpMarker::Connected           => out.push_str(&format!("  '{name}': connected\n")),
            McpMarker::NoTools             => out.push_str(&format!("  '{name}': connected (no tools advertised)\n")),
            McpMarker::SpawnFailed(reason) => out.push_str(&format!("  '{name}': FAILED TO SPAWN — {reason}\n")),
            McpMarker::InitFailed(reason)  => out.push_str(&format!("  '{name}': HANDSHAKE FAILED — {reason}\n")),
            McpMarker::Timeout             => out.push_str(&format!("  '{name}': no marker seen within timeout (run `octo logs {{agent}}` to investigate)\n")),
        }
    }
    out
}

// ── Container-side `command -v` check ───────────────────────────────────────

/// Verify a command is resolvable on `PATH` *inside the lair container*.
/// Replaces the older host-side `command_on_path` — MCP processes are
/// spawned by lair (and by child agents lair spawns), so what matters is
/// the container's PATH, not the operator's shell PATH.
///
/// Returns an error (not just `false`) when the container isn't running,
/// since we can't validate without it and the caller can't proceed anyway.
fn command_in_lair_container(name: &str) -> Result<bool> {
    if !service::is_running() {
        anyhow::bail!(
            "lair container '{}' is not running — start it with `octo init` or `octo reload` so we can verify MCP command availability inside it.",
            service::LAIR_CONTAINER_NAME,
        );
    }
    // Absolute paths: ask the container whether the file exists rather than
    // running `command -v` (which only looks at PATH).
    let probe = if name.contains('/') {
        format!("test -e {}", shell_quote(name))
    } else {
        format!("command -v {} >/dev/null 2>&1", shell_quote(name))
    };
    debug!("[mcp] probing command availability inside lair container: {name}");
    let status = service::docker_exec_status(&["sh", "-c", &probe])
        .context("docker exec command-availability probe")?;
    debug!("[mcp] command '{name}' available in container: {}", status.success());
    Ok(status.success())
}

/// Single-quote a string for safe interpolation into a `sh -c` payload.
fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', r"'\''");
    format!("'{escaped}'")
}

// ── Subcommand handlers ─────────────────────────────────────────────────────

pub async fn list(agent: &str) -> Result<()> {
    let configs = read_mcp(agent)?;
    if configs.is_empty() {
        println!("No MCP servers configured in '{agent}'.");
        return Ok(());
    }
    for c in &configs {
        let args = if c.args.is_empty() { String::new() } else { format!(" {}", c.args.join(" ")) };
        println!("{}: {}{}", c.name, c.command, args);
        for k in c.env.keys() {
            println!("    {k}");
        }
    }
    Ok(())
}

pub async fn add(
    agent: &str,
    name:  &str,
    command: &str,
    args: &[String],
    env_pairs: &[String],
) -> Result<()> {
    let mut configs = read_mcp(agent)?;

    if configs.iter().any(|c| c.name == name) {
        anyhow::bail!("MCP server '{name}' already exists in '{agent}'");
    }

    let mut env = HashMap::new();
    let mut missing: Vec<String> = Vec::new();
    for pair in env_pairs {
        let (k, v) = pair.split_once('=')
            .with_context(|| format!("invalid env pair '{pair}': expected KEY=VALUE"))?;
        match crate::init::expand_host_env(v) {
            Ok(resolved) => { env.insert(k.to_string(), resolved); }
            Err(var)     => missing.push(var),
        }
    }
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        anyhow::bail!(
            "env var(s) not visible to this process: {}. Verify with `env | grep <NAME>` — \
             variables defined in ~/.bashrc must be `export`ed to reach child processes. \
             Otherwise pass literal values.",
            missing.join(", "),
        );
    }

    configs.push(McpServerConfig {
        name:    name.to_string(),
        command: command.to_string(),
        args:    args.to_vec(),
        env,
        url:     None,
        headers: HashMap::new(),
    });

    let names = vec![name.to_string()];
    let since = latest_log_timestamp(agent);

    info!("[mcp] adding server '{name}' to agent '{agent}'");
    println!("→ writing config to '{agent}'");
    write_mcp(agent, &configs)?;

    println!("→ waiting for MCP server to connect (up to 60s)...");
    let results = wait_for_mcp_markers(WaitOpts {
        agent,
        names:    &names,
        timeout:  Duration::from_secs(60),
        since,
    }).await;

    let marker = results.get(name).cloned().unwrap_or(McpMarker::Timeout);
    if !marker.is_success() {
        warn!("[mcp] server '{name}' did not connect; rolling back config for agent '{agent}'");
        configs.retain(|c| c.name != name);
        write_mcp(agent, &configs)?;
    }

    match marker {
        McpMarker::Connected => {
            info!("[mcp] server '{name}' connected on agent '{agent}'");
            println!("MCP server '{name}' connected successfully.");
        }
        McpMarker::NoTools => {
            info!("[mcp] server '{name}' connected (no tools) on agent '{agent}'");
            println!("MCP server '{name}' connected but advertised no tools.");
        }
        McpMarker::SpawnFailed(r) => {
            error!("[mcp] server '{name}' failed to spawn: {r}");
            anyhow::bail!("MCP server '{name}' failed to spawn — {r}");
        }
        McpMarker::InitFailed(r) => {
            error!("[mcp] server '{name}' MCP handshake failed: {r}");
            anyhow::bail!("MCP server '{name}' process started but MCP handshake failed — {r}");
        }
        McpMarker::Timeout => {
            error!("[mcp] server '{name}' did not confirm connection within timeout");
            anyhow::bail!(
                "MCP server '{name}' did not confirm connection within timeout — entry not saved. Run `octo logs {agent}` to investigate."
            );
        }
    }
    Ok(())
}

pub async fn import_from_file(agent: &str, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let entries: Vec<McpServerConfig> = serde_json::from_str(&text)
        .context("parse JSON — expected an array of MCP server objects")?;
    if entries.is_empty() {
        println!("No entries found in '{}'.", path.display());
        return Ok(());
    }

    let mut missing: Vec<String> = Vec::new();
    let resolved: Vec<McpServerConfig> = entries.into_iter().map(|mut e| {
        let expand_map = |m: HashMap<String, String>, missing: &mut Vec<String>| -> HashMap<String, String> {
            m.into_iter().filter_map(|(k, v)| {
                match crate::init::expand_host_env(&v) {
                    Ok(resolved) => Some((k, resolved)),
                    Err(var)     => { missing.push(var); None }
                }
            }).collect()
        };
        e.env     = expand_map(e.env,     &mut missing);
        e.headers = expand_map(e.headers, &mut missing);
        e
    }).collect();

    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        anyhow::bail!(
            "env var(s) not visible to this process: {}. Verify with `env | grep <NAME>` — \
             variables defined in ~/.bashrc must be `export`ed to reach child processes. \
             Otherwise inline the values in '{}'.",
            missing.join(", "),
            path.display(),
        );
    }

    // Preflight: verify every stdio entry's command exists on the lair
    // container's PATH (which is what'll actually try to spawn it). URL-based
    // entries don't spawn a process and are skipped. We do this BEFORE the
    // file write so a bad import never lands in mcp.json.
    let mut missing_commands: Vec<(String, String)> = Vec::new();
    for entry in &resolved {
        if entry.url.is_some() { continue; }
        let cmd = entry.command.trim();
        if cmd.is_empty() {
            anyhow::bail!("MCP server '{}' has neither `command` nor `url`", entry.name);
        }
        match command_in_lair_container(cmd) {
            Ok(true)  => {}
            Ok(false) => missing_commands.push((entry.name.clone(), cmd.to_string())),
            Err(e)    => anyhow::bail!(e),
        }
    }
    if !missing_commands.is_empty() {
        let mut msg = format!(
            "the following MCP server commands are not on the lair container's PATH (container: '{}'):\n",
            service::LAIR_CONTAINER_NAME,
        );
        for (name, cmd) in &missing_commands {
            msg.push_str(&format!("  '{name}' → '{cmd}'\n"));
        }
        msg.push_str(
            "\nThese binaries must exist inside the lair Docker image, not on your shell — \
             MCP servers are spawned by lair, which runs in the container. Either bake the \
             missing tool into a custom image, or switch the entry to an HTTP-based MCP \
             transport (`\"url\": \"...\"`)."
        );
        anyhow::bail!(msg);
    }

    // Snapshot the previous file so we can roll it back if startup fails.
    let previous = std::fs::read_to_string(mcp_path(agent)).ok();
    // lair's hot-reload is name-based: it only (re)spawns servers whose name
    // is new relative to the running pool. A server already in mcp.json stays
    // connected and emits no fresh `connected` marker, so we must only wait
    // on — and gate the import on — servers lair will actually start.
    let previous_names: std::collections::HashSet<String> = previous
        .as_deref()
        .and_then(|t| serde_json::from_str::<Vec<McpServerConfig>>(t).ok())
        .map(|v| v.into_iter().map(|c| c.name).collect())
        .unwrap_or_default();
    let names: Vec<String> = resolved.iter()
        .map(|e| e.name.clone())
        .filter(|n| !previous_names.contains(n))
        .collect();
    let kept: Vec<String> = resolved.iter()
        .map(|e| e.name.clone())
        .filter(|n| previous_names.contains(n))
        .collect();
    // Capture the cutoff timestamp before the write so marker scanning only
    // considers lines lair emits in response to this import.
    let since = latest_log_timestamp(agent);

    info!("[mcp] importing {} server(s) into agent '{agent}' from {}", resolved.len(), path.display());
    println!("Importing {} MCP server(s) into '{agent}' (replacing existing config)...", resolved.len());
    write_mcp(agent, &resolved)?;

    if names.is_empty() {
        println!(
            "Imported successfully — {} server(s) already present and left running: {}",
            kept.len(),
            kept.join(", "),
        );
        return Ok(());
    }

    println!("→ waiting for {} new MCP server(s) to connect (up to 60s)...", names.len());
    let results = wait_for_mcp_markers(WaitOpts {
        agent,
        names:    &names,
        timeout:  Duration::from_secs(60),
        since,
    }).await;

    let mut failures: Vec<String> = names.iter()
        .filter(|n| !results.get(*n).map(|m| m.is_success()).unwrap_or(false))
        .cloned()
        .collect();
    failures.sort();

    if failures.is_empty() {
        info!("[mcp] import into agent '{agent}' succeeded ({} server(s))", names.len());
        println!("Imported successfully:");
        print!("{}", format_marker_report(&results, &names));
        if !kept.is_empty() {
            println!("Already present, left running: {}", kept.join(", "));
        }
        return Ok(());
    }

    // Rollback: restore the previous mcp.json (or delete if there was none).
    error!("[mcp] import into agent '{agent}' failed for {} server(s): {}", failures.len(), failures.join(", "));
    eprintln!("\nMCP startup failures detected:");
    eprint!("{}", format_marker_report(&results, &names));
    warn!("[mcp] rolling back: restoring previous {}", mcp_path(agent).display());
    eprintln!("\nRolling back: restoring previous '{}'.", mcp_path(agent).display());
    match previous {
        Some(text) => crate::init::write_secret_file(&mcp_path(agent), &text)?,
        None       => { let _ = std::fs::remove_file(mcp_path(agent)); }
    }
    anyhow::bail!(
        "import aborted — {} MCP server(s) failed to start: {}",
        failures.len(),
        failures.join(", "),
    );
}

pub async fn remove(agent: &str, name: &str) -> Result<()> {
    let mut configs = read_mcp(agent)?;
    let before = configs.len();
    configs.retain(|c| c.name != name);
    if configs.len() == before {
        anyhow::bail!("MCP server '{name}' not found in '{agent}'");
    }
    write_mcp(agent, &configs)?;
    info!("[mcp] removed server '{name}' from agent '{agent}'");
    println!("Removed MCP server '{name}' from '{agent}'.");
    Ok(())
}

// ── Helpers used by `octo init` for its MCP-seed health-check ───────────────

/// Parse a server-name list from a freshly-written mcp.json. Used by
/// `init::run` to know which markers to scan for in lair's startup logs.
pub fn server_names_from_json(json: &str) -> Result<Vec<String>> {
    let entries: Vec<McpServerConfig> = serde_json::from_str(json)
        .context("parse mcp.json content to extract server names")?;
    Ok(entries.into_iter().map(|e| e.name).collect())
}
