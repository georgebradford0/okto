use std::{
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub mod mcp;
pub use mcp::{McpPool, init_mcp_pool, build_tools_with_mcp, chain_executor_with_mcp};

pub mod noise;
pub use noise::{
    DEV_PUBKEY_BASE32, DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC,
    load_or_generate_keypair, run_noise_proxy, to_base32, from_base32,
    open_noise_tunnel,
};

pub mod app;
pub use app::{
    StreamState, buffer_and_fanout,
    HistMsg, messages_to_history, chat_event_to_wire_json,
    save_messages, load_messages, save_tasks, load_tasks, session_dir,
    parse_ping_id, parse_pong_id,
};

pub mod background;
pub use background::{
    BackgroundCommandParams, BackgroundCommandResult, TaskOutput, TaskRecord, TaskStatus,
    DEFAULT_WAKE_INTERVAL_SECS, MIN_WAKE_INTERVAL_SECS,
    cancel_task, completion_chat_event, finalize_task,
    monitor_process_tool, monitor_progress_message, monitor_progress_text,
    register_task, run_command_in_background_tool, spawn_background_command,
    tasks_wire_json,
};

pub mod relay;
pub use relay::{RelaySigner, notify as relay_notify, send_notification_tool,
    NOTIFY_CATEGORY_AGENT_MESSAGE};

pub mod ssh;
pub use ssh::{ensure_ssh_keypair, SSH_PRIVATE_KEY_FILE, SSH_PUBLIC_KEY_FILE};

pub mod registry;
pub use registry::{AgentRecord, AgentStatus, Registry};

// ── Shared HTTP client ────────────────────────────────────────────────────────

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .http2_keep_alive_interval(std::time::Duration::from_secs(20))
            .http2_keep_alive_timeout(std::time::Duration::from_secs(5))
            .http2_keep_alive_while_idle(true)
            .build()
            .expect("failed to build HTTP client")
    })
}

use serde::{Deserialize, Serialize};
use futures_util::StreamExt;
use tokio::sync::mpsc;

// ── Data directory ────────────────────────────────────────────────────────────

/// Per-role data directory: $OCTO_DATA_DIR if set, otherwise $HOME/.octo.
///
/// Lair runs with `OCTO_DATA_DIR=$HOME/.octo/lair`; each child agent runs with
/// `OCTO_DATA_DIR=$HOME/.octo/agents/<name>/data`. The CLI (running on the
/// operator's host) leaves it unset and gets `$HOME/.octo`.
pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("OCTO_DATA_DIR") {
        PathBuf::from(d)
    } else {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".octo")
    }
}

// ── Config ────────────────────────────────────────────────────────────────────

/// Operator-shared config dir. Always `$HOME/.octo` (or `$OCTO_HOME` if set —
/// only honoured for tests). Independent of `data_dir()` so lair, every child
/// agent, and the CLI all read/write the same `config.json` without bind-mount
/// shenanigans.
pub fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("OCTO_HOME") {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".octo")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Config {
    pub name:              Option<String>,
    pub anthropic_api_key: Option<String>,
    pub openai_api_key:    Option<String>,
    pub model:             Option<String>,
    /// Full chat-completions URL (e.g. `https://api.openai.com/v1/chat/completions`).
    /// Sent verbatim — no path is appended.
    pub api_url:           Option<String>,
    // `gh_token` was removed in favour of plain env-var propagation:
    //   - lair gets `GH_TOKEN` via its container env (in prod, `octo init --env
    //     GH_TOKEN=…`; in dev, start_dev.sh forwards the host shell's value);
    //   - lair's `exec_create_agent` / `register_remote_agent` read it from
    //     `std::env::var("GH_TOKEN")` and forward it to children.
    // Existing config.json files that still carry `gh_token` will deserialize
    // fine (the field is silently dropped by serde because Config does not
    // `deny_unknown_fields`) and the next `octo config set …` rewrites them
    // without it.
    /// Max parent-chain length permitted when an agent spawns a child. A
    /// top-level agent is depth 0; its direct children are depth 1; etc.
    /// The cap applies to the *child being spawned*, so depth 3 means
    /// "great-grandchildren are allowed; great-great-grandchildren are not."
    /// `None` → default 3.
    pub agent_spawn_max_depth:    Option<usize>,
    /// Max number of transitive descendants any single agent is allowed to
    /// have at once. Prevents fork-bomb-style runaway growth. `None` → default 5.
    pub agent_spawn_max_descendants: Option<usize>,
}

/// Resolved spawn-cap pair (depth, descendants). Reads `Config`; supplies
/// defaults if either field is absent.
pub fn resolve_agent_spawn_caps(cfg: &Config) -> (usize, usize) {
    (
        cfg.agent_spawn_max_depth.unwrap_or(3),
        cfg.agent_spawn_max_descendants.unwrap_or(5),
    )
}

// ── API Backend ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub enum ApiBackend {
    Anthropic,
    /// `api_url` is the full chat-completions endpoint, used as the POST target
    /// without modification.
    OpenAi { api_url: String },
}

impl ApiBackend {
    /// Resolve the backend from environment then config.
    /// `OPENAI_API_URL` env var or `config.api_url` → OpenAI-compatible.
    /// Otherwise → Anthropic.
    pub fn resolve() -> Self {
        let url = std::env::var("OPENAI_API_URL").ok().filter(|s| !s.is_empty())
            .or_else(|| read_config().api_url.filter(|s| !s.is_empty()));
        match url {
            Some(u) => {
                info!("[core] using OpenAI-compatible backend: {u}");
                ApiBackend::OpenAi { api_url: u }
            }
            None => {
                debug!("[core] using Anthropic backend");
                ApiBackend::Anthropic
            }
        }
    }
}

pub fn read_config() -> Config {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn effective_repo() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

pub fn write_config(cfg: &Config) {
    let path = config_path();
    fs::create_dir_all(path.parent().unwrap()).ok();
    if let Err(e) = fs::write(&path, serde_json::to_string(cfg).unwrap()) {
        error!("[config] failed to write {}: {e}", path.display());
    } else {
        info!("[config] wrote config to {}", path.display());
    }
}

pub fn resolve_api_key() -> Option<String> {
    // When using an OpenAI-compatible backend, OPENAI_API_KEY takes priority over
    // ANTHROPIC_API_KEY so the correct provider key is sent as the Bearer token.
    let openai_url = std::env::var("OPENAI_API_URL").ok().filter(|s| !s.is_empty())
        .or_else(|| read_config().api_url.filter(|s| !s.is_empty()));

    if openai_url.is_some() {
        std::env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty())
            .or_else(|| {
                let cfg = read_config();
                cfg.openai_api_key.filter(|s| !s.is_empty()).or(cfg.anthropic_api_key)
            })
    } else {
        std::env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty())
            .or_else(|| read_config().anthropic_api_key)
            .or_else(|| read_key_from_shell_files())
    }
}

/// MODEL env var > config.model > default sonnet.
pub fn resolve_model() -> String {
    std::env::var("MODEL").ok().filter(|s| !s.is_empty())
        .or_else(|| read_config().model)
        .unwrap_or_else(|| "claude-sonnet-4-6".to_string())
}

pub fn read_key_from_shell_files() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let candidates = [".zshrc", ".zprofile", ".bash_profile", ".bashrc", ".profile"];
    for file in &candidates {
        let path = format!("{}/{}", home, file);
        if let Ok(contents) = std::fs::read_to_string(&path) {
            for line in contents.lines() {
                let line = line.trim();
                let rest = line
                    .strip_prefix("export ANTHROPIC_API_KEY=")
                    .or_else(|| line.strip_prefix("ANTHROPIC_API_KEY="));
                if let Some(rest) = rest {
                    let val = rest.trim_matches('"').trim_matches('\'').trim().to_string();
                    if !val.is_empty() {
                        return Some(val);
                    }
                }
            }
        }
    }
    None
}

// ── Session ───────────────────────────────────────────────────────────────────

pub struct Session {
    pub messages:      Vec<ApiMessage>,
    pub system_prompt: String,
    pub cwd:           String,
    pub cancel:        CancellationToken,
    /// Connected MCP server clients.  Populated once at session creation and
    /// shared across all turns in the agentic loop.
    pub mcp_pool:      McpPool,
}

// ── API Types ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AnthropicTool {
    pub name:         String,
    pub description:  String,
    pub input_schema: serde_json::Value,
    /// Human-readable phrase shown by clients when the model invokes this tool
    /// (e.g. "Creating agent", "Reading file"). Skipped on the wire — it never
    /// leaves core. Built-ins set it inline; MCP tools get a derived fallback.
    #[serde(skip)]
    pub display_label: Option<String>,
}

/// Look up a human-readable label for `name` in the active tool list, falling
/// back to a name-derived label so MCP tools without a curated `display_label`
/// still get a friendly phrase.
pub fn lookup_display_label(tools: &[AnthropicTool], name: &str) -> Option<String> {
    if let Some(t) = tools.iter().find(|t| t.name == name) {
        if let Some(label) = &t.display_label {
            return Some(label.clone());
        }
    }
    Some(derive_display_label(name))
}

/// Best-effort human-readable label for a tool name we don't have a curated
/// label for (e.g. an MCP tool we discovered at runtime). Strips an optional
/// `server__` MCP prefix, then turns the first underscore-separated word into
/// a present-continuous verb: `create_pull_request` → "Creating pull request".
pub fn derive_display_label(name: &str) -> String {
    let bare = name.rsplit("__").next().unwrap_or(name);
    let mut parts = bare.split('_').filter(|s| !s.is_empty());
    let Some(first) = parts.next() else {
        return name.to_string();
    };
    let verb = if first.ends_with('e') && first.len() > 1 {
        format!("{}ing", &first[..first.len() - 1])
    } else {
        format!("{first}ing")
    };
    let mut out = String::with_capacity(name.len() + 4);
    let mut chars = verb.chars();
    if let Some(c) = chars.next() {
        out.extend(c.to_uppercase());
        out.push_str(chars.as_str());
    }
    for w in parts {
        out.push(' ');
        out.push_str(w);
    }
    out
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiMessage {
    pub role:    String,
    pub content: Vec<ContentBlock>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: Vec<serde_json::Value> },
}

// ── /stream keepalive ─────────────────────────────────────────────────────────

/// How often the server emits a `ping` frame on each /stream WebSocket. Mobile
/// auto-responds with `pong` carrying the same id. Picked to be fast enough that
/// half-open connections (NAT timeout, sleeping device) get evicted within ~30s
/// without burning excessive bandwidth on idle chats.
pub const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Maximum number of unacked pings before the server evicts the WS. Two missed
/// pings means we waited two full intervals (~30s) without hearing back, which
/// is conclusive on a healthy network.
pub const KEEPALIVE_MAX_MISSED: u64 = 2;

// ── Chat Events ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    Ready              { session_id: String, resumed: bool },
    Text               { text: String },
    ToolUse            {
        tool:  String,
        input: serde_json::Value,
        /// Human-readable phrase clients should show in place of `tool` (e.g.
        /// "Creating agent" rather than `create_agent`). Optional for backward
        /// compatibility — older clients fall back to `tool`.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        display: Option<String>,
    },
    ToolOutput         { line: String },
    ToolResult         { tool_use_id: String, content: serde_json::Value },
    Result             { cost_usd: f64, turns: usize, session_id: String, result: Option<String> },
    Error              { message: String },
    Interrupted        { cost_usd: f64 },
    InterruptAck,
    System             { text: String },
    /// Server → client push of the current child-agent list. Lair sends this
    /// on every poller state change.
    Agents             { agents: serde_json::Value },
    /// Server → client push of the per-chat background-task registry. Both lair
    /// and agent send this on /stream open and after every spawn / completion.
    /// Payload is a JSON array of `TaskRecord`-shaped objects.
    Tasks              { tasks: serde_json::Value },
    /// Server → client live notification that a background task's `bg_complete`
    /// row has just been persisted. Mobile renders it as the same chip it would
    /// show after a /history reload, so the user sees the marker between the
    /// pre-spawn and post-completion assistant turns. `task_id` is the stable
    /// dedupe key in case the row also arrives via /history.
    BgComplete         { task_id: String, text: String },
    /// Server → client live notification that a *monitored* background task
    /// produced new output mid-run. Mobile renders it as a progress chip
    /// between turns; the model is separately woken to react to the same
    /// output via a `bg_progress` ApiMessage. `task_id` identifies the task.
    BgProgress         { task_id: String, text: String },
    /// Server → client liveness probe. Client must reply with a Pong within the
    /// keepalive window or the server will drop the connection.
    Ping               { id: u64 },
    /// Client → server reply to a Ping. `id` echoes the Ping's id.
    Pong               { id: u64 },
    /// Client → server start-of-turn message. Carries the user's prompt.
    UserMessage        { text: String },
    /// Client → server interrupt of the current turn.
    Interrupt,
    /// Client → server request to start a stopped child agent by name.
    /// `id` is the agent's `name` from the registry.
    StartAgent         { id: String },
    /// Client → server request to terminate (stop + remove) a child agent.
    TerminateAgent     { id: String },
    /// Client → server request to cancel a running background task by id.
    /// Both lair and agent honour this against their per-chat task registry.
    CancelTask         { id: String },
}

// ── Branch ────────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct Branch {
    pub name:   String,
    pub commit: String,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Utilities ─────────────────────────────────────────────────────────────────

pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag    = false;
    let mut in_script = false;
    let mut tag_buf   = String::new();

    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => { in_tag = true; tag_buf.clear(); }
            '>' => {
                let tag = tag_buf.trim().to_lowercase();
                if tag.starts_with("script") || tag.starts_with("style") {
                    in_script = true;
                } else if tag.starts_with("/script") || tag.starts_with("/style") {
                    in_script = false;
                }
                in_tag = false;
            }
            _ if in_tag      => { tag_buf.push(c); }
            _ if !in_script  => { out.push(c); }
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn resolve_path(p: &str, cwd: &str) -> PathBuf {
    if p.starts_with('/') { PathBuf::from(p) } else { PathBuf::from(cwd).join(p) }
}

/// Per-tool character limits for tool output fed back into the model.
/// Sized to preserve all actionable information while keeping history tokens low.
pub fn tool_output_limit(tool: &str) -> usize {
    match tool {
        // Shell commands and file reads can produce large but meaningful output.
        "bash"        => 10_000,
        "read_file"   => 10_000,
        // Web pages contain lots of useful prose; strip_html already reduces them.
        "web_fetch"   => 10_000,
        // Search results: 10 results × ~400 chars each saturates well under 4 k.
        "web_search"  =>  4_000,
        // Match lists: more than ~6 k of grep hits is noise the model won't act on.
        "grep"        =>  4_000,
        // File-path lists are inherently short.
        "glob"        =>  3_000,
        // Everything else returns a short fixed string; 2 000 is a safe ceiling.
        _             =>  2_000,
    }
}

pub fn truncate_tool_output(s: String, limit: usize) -> String {
    if s.len() <= limit { return s; }
    let boundary = (0..=limit).rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    format!(
        "{}\n[output truncated — {} chars omitted]",
        &s[..boundary],
        s.len() - boundary,
    )
}

pub fn cost_usd(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
) -> f64 {
    let (input_rate, output_rate) = if model.contains("opus") {
        (15.0_f64, 75.0_f64)
    } else if model.contains("haiku") {
        (0.80_f64, 4.0_f64)
    } else {
        // sonnet and any other models
        (3.0_f64, 15.0_f64)
    };
    // Cache writes are billed at 125% of the normal input rate.
    // Cache reads are billed at 10% of the normal input rate.
    (input_tokens as f64 * input_rate
        + output_tokens as f64 * output_rate
        + cache_creation_input_tokens as f64 * input_rate * 1.25
        + cache_read_input_tokens as f64 * input_rate * 0.10)
        / 1_000_000.0
}

// ── Tool Definitions ──────────────────────────────────────────────────────────

pub fn tool_definitions_with_mcp(extra: &[AnthropicTool]) -> Vec<AnthropicTool> {
    let mut tools = tool_definitions();
    let builtin_names: std::collections::HashSet<String> =
        tools.iter().map(|t| t.name.clone()).collect();
    for t in extra {
        if !builtin_names.contains(&t.name) {
            tools.push(t.clone());
        }
    }
    tools
}

pub fn tool_definitions() -> Vec<AnthropicTool> {
    let mut tools = vec![
        AnthropicTool { name: "bash".into(),
            description: "Run a shell command in the repository directory. Returns stdout/stderr.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] }),
            display_label: Some("Running command".into()) },
        AnthropicTool { name: "read_file".into(),
            description: "Read a file. Always use offset+limit to read only the section you need — never read the whole file if you already know the relevant line numbers from grep. offset is 0-based (first line = 0). Lines are returned with 1-based line numbers.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" }, "offset": { "type": "integer", "description": "0-based line index to start reading from" }, "limit": { "type": "integer", "description": "number of lines to return" } }, "required": ["path"] }),
            display_label: Some("Reading file".into()) },
        AnthropicTool { name: "edit_file".into(),
            description: "Replace an exact string in a file. PREFER this over write_file for modifying existing files. old_str must match exactly once.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" }, "old_str": { "type": "string" }, "new_str": { "type": "string" } }, "required": ["path", "old_str", "new_str"] }),
            display_label: Some("Editing file".into()) },
        AnthropicTool { name: "write_file".into(),
            description: "Write a file. Use for creating new files only; prefer edit_file for existing files.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" }, "content": { "type": "string" } }, "required": ["path", "content"] }),
            display_label: Some("Writing file".into()) },
        AnthropicTool { name: "glob".into(),
            description: "Find files matching a glob pattern (e.g. src/**/*.rs).".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "pattern": { "type": "string" } }, "required": ["pattern"] }),
            display_label: Some("Finding files".into()) },
        AnthropicTool { name: "grep".into(),
            description: "Search file contents for a regex pattern. Returns matching lines with file:line numbers. Use context to include surrounding lines. Pass the returned line numbers to read_file offset+limit to read more of that section.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "pattern": { "type": "string" }, "path": { "type": "string" }, "context": { "type": "integer", "description": "number of lines to show before and after each match (like grep -C)" } }, "required": ["pattern"] }),
            display_label: Some("Searching files".into()) },
        AnthropicTool { name: "web_fetch".into(),
            description: "Fetch a URL and return its text content (HTML stripped). Truncated at 50 000 chars.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"] }),
            display_label: Some("Fetching URL".into()) },
    ];
    if std::env::var("BRAVE_API_KEY").ok().filter(|s| !s.is_empty()).is_some() {
        tools.push(AnthropicTool { name: "web_search".into(),
            description: "Search the web via Brave Search.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "query": { "type": "string" } }, "required": ["query"] }),
            display_label: Some("Searching the web".into()) });
    }
    tools
}

// ── Tool Execution ─────────────────────────────────────────────────────────────

pub async fn execute_tool(
    name:             &str,
    input:            &serde_json::Value,
    cwd:              &str,
    tx:               &mpsc::Sender<ChatEvent>,
    cancel:           CancellationToken,
    extra_executor:   Option<&(dyn Fn(String, serde_json::Value)
                               -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
                               + Send + Sync)>,
) -> String {
    let tool_start = std::time::Instant::now();
    debug!("[core/tool] execute '{name}' cwd={cwd}");
    // Race the tool future against the cancel token. On cancel, the in-flight
    // future is dropped — cancel-by-drop gives us a universal interrupt that
    // works for every tool including MCP, at the cost of possibly leaving
    // partial state (e.g. a half-written file). The model will see the
    // synthetic "error: interrupted" result and can clean up on the next turn.
    let result = tokio::select! {
        r = execute_tool_inner(name, input, cwd, tx, cancel.clone(), extra_executor) => r,
        _ = cancel.cancelled() => "error: interrupted".to_string(),
    };
    let elapsed = tool_start.elapsed().as_millis();
    let preview: String = result.chars().take(200).collect();
    info!("[core/tool] '{name}' done in {elapsed}ms ({} chars): {preview}", result.len());
    result
}

async fn execute_tool_inner(
    name:             &str,
    input:            &serde_json::Value,
    cwd:              &str,
    tx:               &mpsc::Sender<ChatEvent>,
    cancel:           CancellationToken,
    extra_executor:   Option<&(dyn Fn(String, serde_json::Value)
                               -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
                               + Send + Sync)>,
) -> String {
    match name {
        "bash" => {
            use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};
            let cmd = input["command"].as_str().unwrap_or("");
            match tokio::process::Command::new("bash")
                .arg("-c").arg(cmd)
                .current_dir(cwd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Err(e) => {
                    error!("[core/tool] bash spawn failed cwd={cwd}: {e}");
                    format!("error: {e}")
                }
                Ok(mut child) => {
                    let stdout_pipe = child.stdout.take().expect("stdout piped");
                    let stderr_pipe = child.stderr.take().expect("stderr piped");
                    let mut stdout_reader = TokioBufReader::new(stdout_pipe).lines();
                    let mut stderr_reader = TokioBufReader::new(stderr_pipe).lines();
                    let mut stdout_buf = String::new();
                    let mut stderr_buf = String::new();
                    loop {
                        tokio::select! {
                            line = stdout_reader.next_line() => match line {
                                Ok(Some(l)) => {
                                    tx.send(ChatEvent::ToolOutput { line: l.clone() }).await.ok();
                                    stdout_buf.push_str(&l);
                                    stdout_buf.push('\n');
                                }
                                _ => break,
                            },
                            line = stderr_reader.next_line() => match line {
                                Ok(Some(l)) => {
                                    tx.send(ChatEvent::ToolOutput { line: format!("[stderr] {l}") }).await.ok();
                                    stderr_buf.push_str(&l);
                                    stderr_buf.push('\n');
                                }
                                _ => break,
                            },
                            _ = cancel.cancelled() => {
                                debug!("[core/tool] bash interrupted, killing child");
                                child.kill().await.ok();
                                return "error: interrupted".to_string();
                            }
                        }
                    }
                    // Drain whichever pipe still has data after select exits.
                    while let Ok(Some(l)) = stdout_reader.next_line().await {
                        tx.send(ChatEvent::ToolOutput { line: l.clone() }).await.ok();
                        stdout_buf.push_str(&l);
                        stdout_buf.push('\n');
                    }
                    while let Ok(Some(l)) = stderr_reader.next_line().await {
                        tx.send(ChatEvent::ToolOutput { line: format!("[stderr] {l}") }).await.ok();
                        stderr_buf.push_str(&l);
                        stderr_buf.push('\n');
                    }
                    child.wait().await.ok();
                    if stderr_buf.is_empty() {
                        stdout_buf
                    } else {
                        format!("{stdout_buf}\n[stderr]: {stderr_buf}")
                    }
                }
            }
        }
        "read_file" => {
            let p      = input["path"].as_str().unwrap_or("");
            let full   = resolve_path(p, cwd);
            let offset = input["offset"].as_u64().unwrap_or(0) as usize;
            let limit  = input["limit"].as_u64().map(|v| v as usize);
            match fs::File::open(&full) {
                Err(e) => {
                    warn!("[core/tool] read_file open failed path={}: {e}", full.display());
                    format!("error: {e}")
                }
                Ok(file) => {
                    let reader = BufReader::new(file);
                    let mut numbered = Vec::new();
                    let mut total = 0usize;
                    for (i, line) in reader.lines().enumerate() {
                        let line = match line {
                            Ok(l)  => l,
                            Err(e) => return format!("error reading line {}: {e}", i + 1),
                        };
                        total += 1;
                        if i < offset { continue; }
                        if let Some(lim) = limit {
                            if numbered.len() >= lim { continue; }
                        }
                        numbered.push(format!("{:>4}→{}", i + 1, line));
                    }
                    let start = offset + 1;
                    let end   = offset + numbered.len();
                    if offset > 0 || limit.is_some() {
                        format!("(lines {start}-{end} of {total})\n{}", numbered.join("\n"))
                    } else {
                        numbered.join("\n")
                    }
                }
            }
        }
        "edit_file" => {
            let p       = input["path"].as_str().unwrap_or("");
            let old_str = input["old_str"].as_str().unwrap_or("");
            let new_str = input["new_str"].as_str().unwrap_or("");
            let full    = resolve_path(p, cwd);
            match fs::read_to_string(&full) {
                Err(e) => {
                    warn!("[core/tool] edit_file read failed path={}: {e}", full.display());
                    format!("error reading file: {e}")
                }
                Ok(content) => {
                    let count = content.matches(old_str).count();
                    if count == 0 {
                        warn!("[core/tool] edit_file old_str not found path={}", full.display());
                        "error: old_str not found in file".to_string()
                    } else if count > 1 {
                        warn!("[core/tool] edit_file old_str matches {count} locations path={}", full.display());
                        format!("error: old_str matches {count} locations — make it more specific")
                    } else {
                        let updated = content.replacen(old_str, new_str, 1);
                        match fs::write(&full, updated) {
                            Ok(_)  => {
                                debug!("[core/tool] edit_file wrote path={}", full.display());
                                "ok".to_string()
                            }
                            Err(e) => {
                                error!("[core/tool] edit_file write failed path={}: {e}", full.display());
                                format!("error writing file: {e}")
                            }
                        }
                    }
                }
            }
        }
        "write_file" => {
            let p       = input["path"].as_str().unwrap_or("");
            let content = input["content"].as_str().unwrap_or("");
            let full    = resolve_path(p, cwd);
            if let Some(parent) = full.parent() { fs::create_dir_all(parent).ok(); }
            match fs::write(&full, content) {
                Ok(_)  => {
                    debug!("[core/tool] write_file wrote {} bytes path={}", content.len(), full.display());
                    "ok".to_string()
                }
                Err(e) => {
                    error!("[core/tool] write_file failed path={}: {e}", full.display());
                    format!("error: {e}")
                }
            }
        }
        "glob" => {
            let pattern      = input["pattern"].as_str().unwrap_or("**/*");
            let base         = PathBuf::from(cwd);
            let full_pattern = format!("{cwd}/{pattern}");
            match glob::glob(&full_pattern) {
                Ok(paths) => paths
                    .filter_map(|p| p.ok())
                    .filter(|p| p.is_file())
                    .map(|p| p.strip_prefix(&base)
                        .map(|r| r.to_string_lossy().to_string())
                        .unwrap_or_else(|_| p.to_string_lossy().to_string()))
                    .collect::<Vec<_>>().join("\n"),
                Err(e) => {
                    warn!("[core/tool] glob pattern invalid '{full_pattern}': {e}");
                    format!("error: {e}")
                }
            }
        }
        "grep" => {
            let pattern = input["pattern"].as_str().unwrap_or("");
            let path    = input["path"].as_str().unwrap_or(".");
            let context = input["context"].as_u64().unwrap_or(0);
            let mut args = vec!["-r", "-n", "--include=*"];
            let ctx_str;
            if context > 0 {
                ctx_str = format!("-C{context}");
                args.push(&ctx_str);
            }
            args.push(pattern);
            args.push(path);
            match tokio::process::Command::new("grep")
                .args(&args)
                .current_dir(cwd).output().await
            {
                Ok(o)  => String::from_utf8_lossy(&o.stdout).to_string(),
                Err(e) => {
                    error!("[core/tool] grep spawn failed cwd={cwd}: {e}");
                    format!("error: {e}")
                }
            }
        }
        "web_fetch" => {
            let url = input["url"].as_str().unwrap_or("");
            if url.is_empty() { return "error: url is required".to_string(); }
            debug!("[core/tool] web_fetch GET {url}");
            match http_client().get(url)
                .header("User-Agent", "Mozilla/5.0 (compatible; octo/1.0)")
                .send().await {
                Err(e)   => {
                    warn!("[core/tool] web_fetch request failed url={url}: {e}");
                    format!("error: {e}")
                }
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        warn!("[core/tool] web_fetch non-2xx url={url} status={status}");
                    }
                    match resp.text().await {
                        Err(e)   => {
                            warn!("[core/tool] web_fetch body read failed url={url}: {e}");
                            format!("error reading response: {e}")
                        }
                        Ok(body) => {
                            let text = strip_html(&body);
                            if status.is_success() { text }
                            else { format!("HTTP {status}\n{text}") }
                        }
                    }
                }
            }
        }
        "web_search" => {
            let query = input["query"].as_str().unwrap_or("");
            if query.is_empty() { return "error: query is required".to_string(); }
            let api_key = match std::env::var("BRAVE_API_KEY").ok().filter(|s| !s.is_empty()) {
                Some(k) => k,
                None    => {
                    warn!("[core/tool] web_search: BRAVE_API_KEY not set");
                    return "error: BRAVE_API_KEY environment variable not set".to_string();
                }
            };
            debug!("[core/tool] web_search query='{query}'");
            match http_client()
                .get("https://api.search.brave.com/res/v1/web/search")
                .query(&[("q", query), ("count", "10")])
                .header("Accept", "application/json")
                .header("X-Subscription-Token", api_key)
                .send().await
            {
                Err(e)   => {
                    warn!("[core/tool] web_search request failed: {e}");
                    format!("error: {e}")
                }
                Ok(resp) => match resp.json::<serde_json::Value>().await {
                    Err(e) => {
                        warn!("[core/tool] web_search response parse failed: {e}");
                        format!("error parsing response: {e}")
                    }
                    Ok(v)  => match v["web"]["results"].as_array() {
                        None        => "no results".to_string(),
                        Some(items) => items.iter().map(|r| {
                            let title = r["title"].as_str().unwrap_or("");
                            let url   = r["url"].as_str().unwrap_or("");
                            let desc  = r["description"].as_str().unwrap_or("");
                            format!("**{title}**\n{url}\n{desc}")
                        }).collect::<Vec<_>>().join("\n\n"),
                    },
                },
            }
        }
        _ => match extra_executor {
            Some(f) => f(name.to_string(), input.clone()).await,
            None    => {
                warn!("[core/tool] unknown tool requested: {name}");
                format!("unknown tool: {name}")
            }
        },
    }
}

// ── Anthropic Streaming ───────────────────────────────────────────────────────

#[derive(Default)]
pub struct StreamUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

/// Compact old turns in the message history to avoid quadratic context growth.
///
/// The most recent `keep_full` tool-result user messages and their paired assistant
/// turns are kept intact. Older pairs are stubbed:
///
/// - Tool-result user messages: content replaced with an outcome+size summary
///   (`[ok — N chars, truncated]` or `[error — N chars, truncated]`).
/// - Paired assistant messages: `Text` blocks replaced with `[truncated]`;
///   `ToolUse` blocks retain `id` and `name` (required for API validity) but
///   the `input` is dropped (`{}`).
pub fn compact_history(messages: &[ApiMessage], keep_full: usize) -> Vec<ApiMessage> {
    // Pre-pass: translate roles that are persisted-only (`bg_complete` /
    // `bg_progress` for background-task injections) into roles the API
    // understands. Done here rather than at the serialiser layer so every
    // backend gets the same behaviour automatically.
    let messages: Vec<ApiMessage> = messages.iter().map(|m| match m.role.as_str() {
        "bg_complete" | "bg_progress" => ApiMessage {
            role:    "user".to_string(),
            content: m.content.clone(),
        },
        _ => m.clone(),
    }).collect();
    let messages = &messages[..];

    // Collect indices of user messages whose content is entirely ToolResult blocks.
    let tool_result_indices: Vec<usize> = messages.iter().enumerate()
        .filter(|(_, m)| m.role == "user" && m.content.iter().all(|b| matches!(b, ContentBlock::ToolResult { .. })))
        .map(|(i, _)| i)
        .collect();

    let cutoff = tool_result_indices.len().saturating_sub(keep_full);

    let old_tool_result: std::collections::HashSet<usize> =
        tool_result_indices[..cutoff].iter().copied().collect();

    // The assistant turn immediately before each old tool-result message is also stale.
    let old_assistant: std::collections::HashSet<usize> =
        old_tool_result.iter().filter_map(|&i| i.checked_sub(1)).collect();

    messages.iter().enumerate().map(|(i, m)| {
        if old_tool_result.contains(&i) {
            // Replace raw tool-result content with an outcome+size stub.
            ApiMessage {
                role: m.role.clone(),
                content: m.content.iter().map(|b| match b {
                    ContentBlock::ToolResult { tool_use_id, content } => {
                        let text = content.first().and_then(|v| v["text"].as_str()).unwrap_or("");
                        let stub = if text.is_empty() {
                            "[empty]".to_string()
                        } else {
                            let outcome = if text.starts_with("error:") || text.starts_with("HTTP ") {
                                "error"
                            } else {
                                "ok"
                            };
                            const PREVIEW: usize = 300;
                            if text.len() <= PREVIEW {
                                text.to_string()
                            } else {
                                let boundary = (0..=PREVIEW).rev()
                                    .find(|&i| text.is_char_boundary(i))
                                    .unwrap_or(0);
                                format!("[{outcome} — {} chars total]\n{}\n…[truncated]",
                                    text.len(), &text[..boundary])
                            }
                        };
                        ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: vec![serde_json::json!({"type":"text","text":stub})],
                        }
                    }
                    other => other.clone(),
                }).collect(),
            }
        } else if old_assistant.contains(&i) {
            // Stub text blocks; preserve ToolUse id+name so API structure stays valid.
            ApiMessage {
                role: m.role.clone(),
                content: m.content.iter().map(|b| match b {
                    ContentBlock::Text { text } => {
                        const PREVIEW: usize = 200;
                        ContentBlock::Text {
                            text: if text.len() <= PREVIEW {
                                text.clone()
                            } else {
                                let boundary = (0..=PREVIEW).rev()
                                    .find(|&i| text.is_char_boundary(i))
                                    .unwrap_or(0);
                                format!("{}…[truncated]", &text[..boundary])
                            }
                        }
                    }
                    ContentBlock::ToolUse { id, name, .. } =>
                        ContentBlock::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: serde_json::Value::Object(serde_json::Map::new()),
                        },
                    other => other.clone(),
                }).collect(),
            }
        } else {
            m.clone()
        }
    }).collect()
}

// ── OpenAI compatibility helpers ──────────────────────────────────────────────

fn tools_to_openai(tools: &[AnthropicTool]) -> Vec<serde_json::Value> {
    tools.iter().map(|t| serde_json::json!({
        "type": "function",
        "function": {
            "name":        t.name,
            "description": t.description,
            "parameters":  t.input_schema,
        }
    })).collect()
}

/// Convert internal Anthropic-format messages to OpenAI chat messages.
/// The system prompt is prepended as a `role: system` message.
fn messages_to_openai(system: &str, messages: &[ApiMessage]) -> Vec<serde_json::Value> {
    let mut out = vec![serde_json::json!({"role": "system", "content": system})];
    for msg in messages {
        match msg.role.as_str() {
            "user" => {
                // Text blocks → user message; tool_result blocks → tool messages.
                let texts: Vec<&str> = msg.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                if !texts.is_empty() {
                    out.push(serde_json::json!({"role": "user", "content": texts.join("\n")}));
                }
                for b in &msg.content {
                    if let ContentBlock::ToolResult { tool_use_id, content } = b {
                        let text = content.first()
                            .and_then(|v| v["text"].as_str())
                            .unwrap_or("");
                        out.push(serde_json::json!({
                            "role":         "tool",
                            "tool_call_id": tool_use_id,
                            "content":      text,
                        }));
                    }
                }
            }
            "assistant" => {
                let text: String = msg.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect::<Vec<_>>().join("");
                let tool_calls: Vec<serde_json::Value> = msg.content.iter()
                    .filter_map(|b| {
                        if let ContentBlock::ToolUse { id, name, input } = b {
                            Some(serde_json::json!({
                                "id":   id,
                                "type": "function",
                                "function": {
                                    "name":      name,
                                    "arguments": input.to_string(),
                                }
                            }))
                        } else { None }
                    }).collect();
                let mut m = serde_json::json!({"role": "assistant", "content": serde_json::Value::Null});
                if !text.is_empty()        { m["content"]    = serde_json::json!(text); }
                if !tool_calls.is_empty()  { m["tool_calls"] = serde_json::json!(tool_calls); }
                out.push(m);
            }
            _ => {}
        }
    }
    out
}

// ── SSE streaming helpers ─────────────────────────────────────────────────────

/// Pull the next `event: ... \n data: ...\n\n` block out of `buffer` if one is
/// fully present. Drains the consumed bytes from `buffer` on success. Returns
/// `(event_type, data)` where event_type may be empty (default per RFC).
fn pop_sse_event(buffer: &mut String) -> Option<(String, String)> {
    // SSE events are terminated by a blank line. Tolerate \r\n\r\n and \n\n.
    let term_idx = buffer.find("\n\n").map(|i| (i, 2))
        .or_else(|| buffer.find("\r\n\r\n").map(|i| (i, 4)))?;
    let (idx, term_len) = term_idx;
    let event_str: String = buffer[..idx].to_string();
    buffer.drain(..idx + term_len);

    let mut event_type = String::new();
    let mut data       = String::new();
    for line in event_str.lines() {
        if let Some(rest) = line.strip_prefix("event: ").or_else(|| line.strip_prefix("event:")) {
            event_type = rest.trim_start().to_string();
        } else if let Some(rest) = line.strip_prefix("data: ").or_else(|| line.strip_prefix("data:")) {
            if !data.is_empty() { data.push('\n'); }
            data.push_str(rest.trim_start_matches(' '));
        }
    }
    Some((event_type, data))
}

#[derive(Default)]
struct StreamingBlock {
    kind:           String, // "text" | "tool_use"
    text:           String,
    id:             String,
    name:           String,
    input_json_str: String,
}

/// Stream Anthropic /v1/messages SSE response, emitting Text deltas live and
/// accumulating tool_use input JSON for end-of-block emission.
async fn stream_anthropic(
    response: reqwest::Response,
    cancel:   &CancellationToken,
    tx:       &mpsc::Sender<ChatEvent>,
    tools:    &[AnthropicTool],
) -> Result<(Vec<ContentBlock>, String, StreamUsage), String> {
    let mut bytes_stream = response.bytes_stream();
    let mut buffer       = String::new();
    let mut current: Option<StreamingBlock> = None;
    let mut blocks: Vec<ContentBlock>       = Vec::new();
    let mut stop_reason = "end_turn".to_string();
    let mut usage       = StreamUsage::default();

    loop {
        let chunk_res = tokio::select! {
            c = bytes_stream.next() => c,
            _ = cancel.cancelled()  => return Err("__interrupted__".to_string()),
        };
        let Some(chunk_res) = chunk_res else { break };
        let chunk = chunk_res.map_err(|e| e.to_string())?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some((event_type, data)) = pop_sse_event(&mut buffer) {
            if data.is_empty() { continue; }
            let v: serde_json::Value = match serde_json::from_str(&data) {
                Ok(v)  => v,
                Err(_) => continue,
            };
            match event_type.as_str() {
                "message_start" => {
                    let u = &v["message"]["usage"];
                    usage.input_tokens                = u["input_tokens"].as_u64().unwrap_or(0);
                    usage.cache_creation_input_tokens = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                    usage.cache_read_input_tokens     = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                }
                "content_block_start" => {
                    let cb = &v["content_block"];
                    current = Some(StreamingBlock {
                        kind: cb["type"].as_str().unwrap_or("").to_string(),
                        id:   cb["id"].as_str().unwrap_or("").to_string(),
                        name: cb["name"].as_str().unwrap_or("").to_string(),
                        ..Default::default()
                    });
                }
                "content_block_delta" => {
                    let delta = &v["delta"];
                    let dtype = delta["type"].as_str().unwrap_or("");
                    if let Some(cur) = current.as_mut() {
                        match dtype {
                            "text_delta" => {
                                let chunk = delta["text"].as_str().unwrap_or("").to_string();
                                if !chunk.is_empty() {
                                    cur.text.push_str(&chunk);
                                    tx.send(ChatEvent::Text { text: chunk }).await.ok();
                                }
                            }
                            "input_json_delta" => {
                                cur.input_json_str.push_str(delta["partial_json"].as_str().unwrap_or(""));
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_stop" => {
                    if let Some(cur) = current.take() {
                        match cur.kind.as_str() {
                            "text" if !cur.text.is_empty() => {
                                blocks.push(ContentBlock::Text { text: cur.text });
                            }
                            "tool_use" => {
                                let input: serde_json::Value = if cur.input_json_str.is_empty() {
                                    serde_json::json!({})
                                } else {
                                    serde_json::from_str(&cur.input_json_str).unwrap_or(serde_json::json!({}))
                                };
                                info!("[core/stream/anthropic] tool_use name={} id={}", cur.name, cur.id);
                                tx.send(ChatEvent::ToolUse {
                                    tool:    cur.name.clone(),
                                    input:   input.clone(),
                                    display: lookup_display_label(tools, &cur.name),
                                }).await.ok();
                                blocks.push(ContentBlock::ToolUse {
                                    id: cur.id, name: cur.name, input,
                                });
                            }
                            _ => {}
                        }
                    }
                }
                "message_delta" => {
                    if let Some(sr) = v["delta"]["stop_reason"].as_str() {
                        stop_reason = sr.to_string();
                    }
                    if let Some(out) = v["usage"]["output_tokens"].as_u64() {
                        usage.output_tokens = out;
                    }
                }
                "error" => {
                    let msg = v["error"]["message"].as_str().unwrap_or("unknown streaming error");
                    error!("[core/stream/anthropic] API stream error: {msg}");
                    return Err(format!("API stream error: {msg}"));
                }
                _ => {} // message_stop, ping — no-op
            }
        }
    }

    Ok((blocks, stop_reason, usage))
}

/// Stream OpenAI-compatible /chat/completions SSE response, emitting Text deltas
/// live and accumulating tool_call arguments JSON for end-of-stream emission.
async fn stream_openai(
    response: reqwest::Response,
    cancel:   &CancellationToken,
    tx:       &mpsc::Sender<ChatEvent>,
    tools:    &[AnthropicTool],
) -> Result<(Vec<ContentBlock>, String, StreamUsage), String> {
    let mut bytes_stream = response.bytes_stream();
    let mut buffer       = String::new();
    let mut text_accum   = String::new();
    // Tool calls keyed by index (some providers stream by index; some by id).
    // We accumulate name once and arguments incrementally.
    #[derive(Default, Clone)]
    struct ToolCallAccum { id: String, name: String, args: String }
    let mut tool_calls: Vec<ToolCallAccum> = Vec::new();
    let mut stop_reason  = "end_turn".to_string();
    let mut usage        = StreamUsage::default();

    loop {
        let chunk_res = tokio::select! {
            c = bytes_stream.next() => c,
            _ = cancel.cancelled()  => return Err("__interrupted__".to_string()),
        };
        let Some(chunk_res) = chunk_res else { break };
        let chunk = chunk_res.map_err(|e| e.to_string())?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some((_etype, data)) = pop_sse_event(&mut buffer) {
            if data == "[DONE]" || data.is_empty() { continue; }
            let v: serde_json::Value = match serde_json::from_str(&data) {
                Ok(v)  => v,
                Err(_) => continue,
            };
            // OpenAI streams chat completions chunks; usage lands in the final chunk
            // when stream_options.include_usage is set.
            if let Some(u) = v.get("usage") {
                if let Some(p) = u["prompt_tokens"].as_u64()     { usage.input_tokens  = p; }
                if let Some(c) = u["completion_tokens"].as_u64() { usage.output_tokens = c; }
            }
            let Some(choices) = v["choices"].as_array() else { continue };
            let Some(choice)  = choices.first()                else { continue };
            let delta = &choice["delta"];

            if let Some(content) = delta["content"].as_str() {
                if !content.is_empty() {
                    text_accum.push_str(content);
                    tx.send(ChatEvent::Text { text: content.to_string() }).await.ok();
                }
            }
            if let Some(tcs) = delta["tool_calls"].as_array() {
                for tc in tcs {
                    let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                    while tool_calls.len() <= idx { tool_calls.push(ToolCallAccum::default()); }
                    let acc = &mut tool_calls[idx];
                    if let Some(id) = tc["id"].as_str() {
                        if !id.is_empty() { acc.id = id.to_string(); }
                    }
                    if let Some(n) = tc["function"]["name"].as_str() {
                        if !n.is_empty() { acc.name = n.to_string(); }
                    }
                    if let Some(a) = tc["function"]["arguments"].as_str() {
                        acc.args.push_str(a);
                    }
                }
            }
            if let Some(fr) = choice["finish_reason"].as_str() {
                stop_reason = if fr == "tool_calls" { "tool_use".to_string() } else { fr.to_string() };
            }
        }
    }

    let mut blocks: Vec<ContentBlock> = Vec::new();
    if !text_accum.is_empty() {
        blocks.push(ContentBlock::Text { text: text_accum });
    }
    for tc in tool_calls {
        if tc.name.is_empty() { continue; }
        let input: serde_json::Value = if tc.args.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&tc.args).unwrap_or(serde_json::json!({}))
        };
        info!("[core/stream/openai] tool_use name={} id={}", tc.name, tc.id);
        tx.send(ChatEvent::ToolUse {
            tool:    tc.name.clone(),
            input:   input.clone(),
            display: lookup_display_label(tools, &tc.name),
        }).await.ok();
        blocks.push(ContentBlock::ToolUse { id: tc.id, name: tc.name, input });
    }
    Ok((blocks, stop_reason, usage))
}

// ── call_turn ─────────────────────────────────────────────────────────────────

pub async fn call_turn(
    messages:    &[ApiMessage],
    system:      &str,
    model:       &str,
    api_key:     &str,
    cancel:      &CancellationToken,
    tx:          &mpsc::Sender<ChatEvent>,
    extra_tools: &[AnthropicTool],
    backend:     &ApiBackend,
) -> Result<(Vec<ContentBlock>, String, StreamUsage), String> {
    let all_tools = tool_definitions_with_mcp(extra_tools);
    let compacted = compact_history(messages, 20);
    let orig_len = messages.len();
    let compact_len = compacted.len();
    debug!(
        "[core/call_turn] model={model} messages={orig_len} (compacted={compact_len}) tools={}",
        all_tools.len()
    );

    match backend {
        ApiBackend::Anthropic => {
            let mut tools_json: Vec<serde_json::Value> = all_tools
                .iter().map(|t| serde_json::to_value(t).unwrap()).collect();
            if let Some(last) = tools_json.last_mut() {
                last["cache_control"] = serde_json::json!({"type": "ephemeral"});
            }

            let mut messages_json: Vec<serde_json::Value> = compacted
                .iter().map(|m| serde_json::to_value(m).unwrap()).collect();
            let n = messages_json.len();
            if n >= 2 {
                let candidates: Vec<usize> = if n < 4 { vec![n - 2] } else { vec![n - 2, n / 2] };
                let mut seen = std::collections::HashSet::new();
                for idx in candidates {
                    if seen.insert(idx) {
                        if let Some(content) = messages_json[idx]["content"].as_array_mut() {
                            if let Some(last_block) = content.last_mut() {
                                last_block["cache_control"] = serde_json::json!({"type": "ephemeral"});
                            }
                        }
                    }
                }
            }

            let body = serde_json::json!({
                "model":      model,
                "max_tokens": 8192,
                "stream":     true,
                "system":     [{"type":"text","text":system,"cache_control":{"type":"ephemeral"}}],
                "tools":      tools_json,
                "messages":   messages_json,
            });

            let response = tokio::select! {
                res = http_client()
                    .post("https://api.anthropic.com/v1/messages")
                    .header("x-api-key",         api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header("anthropic-beta",    "prompt-caching-2024-07-31")
                    .header("content-type",      "application/json")
                    .header("accept",            "text/event-stream")
                    .json(&body).send() => res.map_err(|e| e.to_string())?,
                _ = cancel.cancelled() => return Err("__interrupted__".to_string()),
            };

            if !response.status().is_success() {
                let status = response.status();
                let text   = response.text().await.unwrap_or_default();
                error!("[core/call_turn] anthropic API error {status}: {text}");
                return Err(format!("API error {status}: {text}"));
            }
            if cancel.is_cancelled() { return Err("__interrupted__".to_string()); }

            let (blocks, stop_reason, stream_usage) = stream_anthropic(response, cancel, tx, &all_tools).await?;
            info!(
                "[core/call_turn] stop_reason={stop_reason} in={} out={} cache_create={} cache_read={}",
                stream_usage.input_tokens,
                stream_usage.output_tokens,
                stream_usage.cache_creation_input_tokens,
                stream_usage.cache_read_input_tokens,
            );
            Ok((blocks, stop_reason, stream_usage))
        }

        ApiBackend::OpenAi { api_url } => {
            let tools_json  = tools_to_openai(&all_tools);
            let messages_oa = messages_to_openai(system, &compacted);

            let body = serde_json::json!({
                "model":      model,
                "max_tokens": 8192,
                "stream":     true,
                "stream_options": { "include_usage": true },
                "tools":      tools_json,
                "messages":   messages_oa,
            });

            let response = tokio::select! {
                res = http_client()
                    .post(api_url)
                    .header("Authorization",  format!("Bearer {api_key}"))
                    .header("content-type",   "application/json")
                    .header("accept",         "text/event-stream")
                    .json(&body).send() => res.map_err(|e| e.to_string())?,
                _ = cancel.cancelled() => return Err("__interrupted__".to_string()),
            };

            if !response.status().is_success() {
                let status = response.status();
                let text   = response.text().await.unwrap_or_default();
                error!("[core/call_turn/openai] API error {status}: {text}");
                return Err(format!("API error {status}: {text}"));
            }
            if cancel.is_cancelled() { return Err("__interrupted__".to_string()); }

            let (blocks, stop_reason, stream_usage) = stream_openai(response, cancel, tx, &all_tools).await?;
            info!(
                "[core/call_turn/openai] stop_reason={stop_reason} in={} out={}",
                stream_usage.input_tokens, stream_usage.output_tokens,
            );
            Ok((blocks, stop_reason, stream_usage))
        }
    }
}

/// Send a message and run the tool loop until Claude stops with end_turn.
/// Returns (final_text, total_cost_usd, updated_messages).
/// MCP is disabled; only built-in tools are available.
/// If `event_tx` is Some, Text and ToolUse events are forwarded to the caller.
/// Passing a cancelled `CancellationToken` causes an early return.
pub async fn send_message(
    mut messages:   Vec<ApiMessage>,
    system:         &str,
    model:          &str,
    api_key:        &str,
    cwd:            &str,
    event_tx:       Option<mpsc::Sender<ChatEvent>>,
    cancel:         CancellationToken,
    extra_tools:    &[AnthropicTool],
    extra_executor: Option<Arc<dyn Fn(String, serde_json::Value)
                            -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
                            + Send + Sync>>,
) -> Result<(String, f64, Vec<ApiMessage>), (String, Vec<ApiMessage>)> {
    let backend    = ApiBackend::resolve();
    let mut total_cost = 0.0f64;
    let mut last_text  = String::new();

    let (dummy_tx, _) = mpsc::channel::<ChatEvent>(1);
    let tx = event_tx.unwrap_or(dummy_tx);

    let all_tools = tool_definitions_with_mcp(extra_tools);
    debug!("[core/send_message] starting model={model} messages={} cwd={cwd}", messages.len());

    let mut turn = 0usize;
    loop {
        turn += 1;
        if cancel.is_cancelled() {
            tx.send(ChatEvent::InterruptAck).await.ok();
            return Ok((last_text, total_cost, messages));
        }
        let compacted = compact_history(&messages, 20);

        // Per-call request: stream the response and emit ChatEvent::Text deltas
        // live (mobile renders typewriter-style). stream_anthropic / stream_openai
        // also emit ChatEvent::ToolUse once each tool_use block finishes assembling.
        let (blocks, stop_reason, usage) = match &backend {
            ApiBackend::Anthropic => {
                let mut tools_json: Vec<serde_json::Value> = all_tools
                    .iter().map(|t| serde_json::to_value(t).unwrap()).collect();
                if let Some(last) = tools_json.last_mut() {
                    last["cache_control"] = serde_json::json!({"type": "ephemeral"});
                }
                let mut messages_json: Vec<serde_json::Value> = compacted.iter()
                    .map(|m| serde_json::to_value(m).unwrap()).collect();
                let n = messages_json.len();
                if n >= 2 {
                    let candidates: Vec<usize> = if n < 4 { vec![n - 2] } else { vec![n - 2, n / 2] };
                    let mut seen = std::collections::HashSet::new();
                    for idx in candidates {
                        if seen.insert(idx) {
                            if let Some(content) = messages_json[idx]["content"].as_array_mut() {
                                if let Some(last_block) = content.last_mut() {
                                    last_block["cache_control"] = serde_json::json!({"type": "ephemeral"});
                                }
                            }
                        }
                    }
                }
                let body = serde_json::json!({
                    "model":      model,
                    "max_tokens": 8192,
                    "stream":     true,
                    "system":     [{"type":"text","text":system,"cache_control":{"type":"ephemeral"}}],
                    "tools":      tools_json,
                    "messages":   messages_json,
                });
                let response = tokio::select! {
                    res = http_client()
                        .post("https://api.anthropic.com/v1/messages")
                        .header("x-api-key",         api_key)
                        .header("anthropic-version", "2023-06-01")
                        .header("anthropic-beta",    "prompt-caching-2024-07-31")
                        .header("content-type",      "application/json")
                        .header("accept",            "text/event-stream")
                        .json(&body).send() => res.map_err(|e| (e.to_string(), messages.clone()))?,
                    _ = cancel.cancelled() => {
                        tx.send(ChatEvent::InterruptAck).await.ok();
                        return Ok((last_text, total_cost, messages));
                    }
                };
                if !response.status().is_success() {
                    let status = response.status();
                    let text   = response.text().await.unwrap_or_default();
                    error!("[send_message] turn={turn} anthropic API error {status}: {text}");
                    return Err((format!("API error {status}: {text}"), messages));
                }
                let (blocks, stop_reason, usage) = match stream_anthropic(response, &cancel, &tx, &all_tools).await {
                    Ok(v) => v,
                    Err(e) if e == "__interrupted__" => {
                        tx.send(ChatEvent::InterruptAck).await.ok();
                        return Ok((last_text, total_cost, messages));
                    }
                    Err(e) => {
                        error!("[send_message] turn={turn} anthropic stream error: {e}");
                        return Err((e, messages));
                    }
                };
                info!(
                    "[send_message] turn={turn} stop_reason={stop_reason} in={} out={} cache_create={} cache_read={}",
                    usage.input_tokens, usage.output_tokens,
                    usage.cache_creation_input_tokens, usage.cache_read_input_tokens,
                );
                total_cost += cost_usd(
                    model,
                    usage.input_tokens, usage.output_tokens,
                    usage.cache_creation_input_tokens, usage.cache_read_input_tokens,
                );
                (blocks, stop_reason, usage)
            }

            ApiBackend::OpenAi { api_url } => {
                let tools_json  = tools_to_openai(&all_tools);
                let messages_oa = messages_to_openai(system, &compacted);
                let body = serde_json::json!({
                    "model":      model,
                    "max_tokens": 8192,
                    "stream":     true,
                    "stream_options": { "include_usage": true },
                    "tools":      tools_json,
                    "messages":   messages_oa,
                });
                let response = tokio::select! {
                    res = http_client()
                        .post(api_url)
                        .header("Authorization", format!("Bearer {api_key}"))
                        .header("content-type",  "application/json")
                        .header("accept",        "text/event-stream")
                        .json(&body).send() => res.map_err(|e| (e.to_string(), messages.clone()))?,
                    _ = cancel.cancelled() => {
                        tx.send(ChatEvent::InterruptAck).await.ok();
                        return Ok((last_text, total_cost, messages));
                    }
                };
                if !response.status().is_success() {
                    let status = response.status();
                    let text   = response.text().await.unwrap_or_default();
                    error!("[send_message/openai] turn={turn} API error {status}: {text}");
                    return Err((format!("API error {status}: {text}"), messages));
                }
                let (blocks, stop_reason, usage) = match stream_openai(response, &cancel, &tx, &all_tools).await {
                    Ok(v) => v,
                    Err(e) if e == "__interrupted__" => {
                        tx.send(ChatEvent::InterruptAck).await.ok();
                        return Ok((last_text, total_cost, messages));
                    }
                    Err(e) => {
                        error!("[send_message/openai] turn={turn} stream error: {e}");
                        return Err((e, messages));
                    }
                };
                info!(
                    "[send_message/openai] turn={turn} stop_reason={stop_reason} in={} out={}",
                    usage.input_tokens, usage.output_tokens,
                );
                // Cost tracking skipped for OpenAI-compatible backends (no fixed pricing).
                (blocks, stop_reason, usage)
            }
        };
        let _ = usage; // tokens already attributed above

        // Collect text + tool_uses out of the streamed blocks. Events were already
        // emitted live by stream_anthropic / stream_openai — don't re-emit here.
        let mut text_buf  = String::new();
        let mut tool_uses: Vec<(String, String, serde_json::Value)> = Vec::new();
        for block in &blocks {
            match block {
                ContentBlock::Text { text }                  => text_buf.push_str(text),
                ContentBlock::ToolUse { id, name, input }    => tool_uses.push((id.clone(), name.clone(), input.clone())),
                _                                            => {}
            }
        }

        messages.push(ApiMessage { role: "assistant".to_string(), content: blocks });

        if stop_reason != "tool_use" || tool_uses.is_empty() {
            info!("[core/send_message] done turns={turn} cost=${total_cost:.4}");
            return Ok((text_buf, total_cost, messages));
        }

        last_text = text_buf;

        let mut results  = Vec::new();
        let mut ack_sent = false;
        for (id, name, input) in tool_uses {
            if cancel.is_cancelled() {
                if !ack_sent {
                    tx.send(ChatEvent::InterruptAck).await.ok();
                    ack_sent = true;
                }
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id,
                    content:     vec![serde_json::json!({"type":"text","text":"interrupted"})],
                });
                continue;
            }
            let result = truncate_tool_output(
                execute_tool(&name, &input, cwd, &tx, cancel.clone(), extra_executor.as_deref()).await,
                tool_output_limit(&name),
            );
            let result_preview = result.chars().take(200).collect::<String>();
            info!("[send_message] turn={turn} tool_result name={name} ({} chars): {result_preview}", result.len());
            results.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content:     vec![serde_json::json!({"type":"text","text":result})],
            });
        }

        messages.push(ApiMessage { role: "user".to_string(), content: results });
    }
}

// ── Agentic Loop ──────────────────────────────────────────────────────────────

pub async fn run_agentic_loop(
    session:        Arc<Mutex<Session>>,
    session_id:     String,
    api_key:        String,
    model:          String,
    tx:             mpsc::Sender<ChatEvent>,
    extra_tools:    Vec<AnthropicTool>,
    extra_executor: Option<Arc<dyn Fn(String, serde_json::Value)
                            -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
                            + Send + Sync>>,
) {
    info!("[core/agentic_loop] starting session_id={session_id} model={model}");
    let backend = ApiBackend::resolve();

    let mut turns                        = 0usize;
    let mut total_input                  = 0u64;
    let mut total_output                 = 0u64;
    let mut total_cache_creation_input   = 0u64;
    let mut total_cache_read_input       = 0u64;

    let partial_cost = |ti, to, tcc, tcr| match &backend {
        ApiBackend::Anthropic => cost_usd(&model, ti, to, tcc, tcr),
        ApiBackend::OpenAi { .. } => 0.0,
    };

    const MAX_TURNS: usize = 100;

    loop {
        if turns >= MAX_TURNS {
            error!("[core/agentic_loop] session_id={session_id} hit MAX_TURNS={MAX_TURNS}, aborting");
            tx.send(ChatEvent::Error {
                message: format!("Stopped after {MAX_TURNS} turns to prevent runaway loop"),
            }).await.ok();
            return;
        }

        let (messages, system, cwd, cancel) = {
            let s = session.lock().unwrap();
            (s.messages.clone(), s.system_prompt.clone(), s.cwd.clone(), s.cancel.clone())
        };

        info!("[core/agentic_loop] session_id={session_id} turn {} messages={}", turns + 1, messages.len());

        if cancel.is_cancelled() {
            info!("[core/agentic_loop] session_id={session_id} cancelled before turn");
            tx.send(ChatEvent::InterruptAck).await.ok();
            tx.send(ChatEvent::Interrupted { cost_usd: partial_cost(total_input, total_output, total_cache_creation_input, total_cache_read_input) }).await.ok();
            return;
        }

        match call_turn(&messages, &system, &model, &api_key, &cancel, &tx, &extra_tools, &backend).await {
            Err(e) if e == "__interrupted__" => {
                info!("[core/agentic_loop] session_id={session_id} interrupted");
                tx.send(ChatEvent::InterruptAck).await.ok();
                tx.send(ChatEvent::Interrupted { cost_usd: partial_cost(total_input, total_output, total_cache_creation_input, total_cache_read_input) }).await.ok();
                return;
            }
            Err(e) => {
                error!("[core/agentic_loop] session_id={session_id} error: {e}");
                tx.send(ChatEvent::Error { message: e }).await.ok();
                return;
            }
            Ok((blocks, stop_reason, usage)) => {
                turns                      += 1;
                total_input                += usage.input_tokens;
                total_output               += usage.output_tokens;
                total_cache_creation_input += usage.cache_creation_input_tokens;
                total_cache_read_input     += usage.cache_read_input_tokens;

                {
                    let mut s = session.lock().unwrap();
                    s.messages.push(ApiMessage { role: "assistant".to_string(), content: blocks.clone() });
                }

                if stop_reason != "tool_use" {
                    let cost = partial_cost(total_input, total_output, total_cache_creation_input, total_cache_read_input);
                    info!(
                        "[core/agentic_loop] session_id={session_id} done turns={turns} cost=${cost:.4}"
                    );
                    tx.send(ChatEvent::Result {
                        cost_usd: cost, turns, session_id: session_id.clone(), result: None,
                    }).await.ok();
                    return;
                }

                let mut tool_results: Vec<ContentBlock> = Vec::new();
                for block in &blocks {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        if cancel.is_cancelled() {
                            // Keep history valid — synthetic result for each orphaned tool_use.
                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![serde_json::json!({"type":"text","text":"interrupted"})],
                            });
                            continue;
                        }
                        let result = truncate_tool_output(
                            execute_tool(name, input, &cwd, &tx, cancel.clone(), extra_executor.as_deref()).await,
                            tool_output_limit(name),
                        );
                        let result_preview: String = result.chars().take(200).collect();
                        info!(
                            "[core/agentic_loop] session_id={session_id} tool_result name={name} ({} chars): {result_preview}",
                            result.len()
                        );
                        tx.send(ChatEvent::ToolResult {
                            tool_use_id: id.clone(),
                            content: serde_json::Value::String(result.clone()),
                        }).await.ok();
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: vec![serde_json::json!({"type":"text","text":result})],
                        });
                    }
                }

                // If cancelled mid-tool-loop, flush results and stop.
                if cancel.is_cancelled() {
                    {
                        let mut s = session.lock().unwrap();
                        s.messages.push(ApiMessage { role: "user".to_string(), content: tool_results });
                    }
                    tx.send(ChatEvent::Interrupted { cost_usd: partial_cost(total_input, total_output, total_cache_creation_input, total_cache_read_input) }).await.ok();
                    return;
                }

                {
                    let mut s = session.lock().unwrap();
                    s.messages.push(ApiMessage { role: "user".to_string(), content: tool_results });
                }
            }
        }
    }
}

// ── Startup Prompt ────────────────────────────────────────────────────────────

/// Run the agentic loop once with `prompt` as the sole user message, logging
/// all output to stdout.  Returns when the loop completes (or errors).
/// Intended to be called at container startup before accepting connections.
pub async fn run_startup_prompt(
    prompt:  &str,
    session: Arc<Mutex<Session>>,
    api_key: &str,
    model:   &str,
) {
    tracing::info!("[startup] running startup prompt ({} chars)", prompt.len());

    {
        let mut s = session.lock().unwrap();
        s.messages.push(ApiMessage {
            role:    "user".to_string(),
            content: vec![ContentBlock::Text { text: prompt.to_string() }],
        });
    }

    let (tx, mut rx) = mpsc::channel::<ChatEvent>(256);
    let session_c = session.clone();
    let api_key_s = api_key.to_string();
    let model_s   = model.to_string();

    let handle = tokio::spawn(async move {
        run_agentic_loop(session_c, "startup".to_string(), api_key_s, model_s, tx, vec![], None).await;
    });

    while let Some(event) = rx.recv().await {
        match &event {
            ChatEvent::Text { text } => print!("{text}"),
            ChatEvent::ToolUse { tool, input, .. } => {
                tracing::info!("[startup] tool_use tool={tool} input={input}");
            }
            ChatEvent::ToolResult { content, .. } => {
                let preview = content.as_str().map(|s| s.chars().take(120).collect::<String>()).unwrap_or_default();
                tracing::info!("[startup] tool_result: {preview}");
            }
            ChatEvent::Error { message } => {
                tracing::error!("[startup] error: {message}");
            }
            ChatEvent::Result { cost_usd, turns, .. } => {
                tracing::info!("[startup] complete turns={turns} cost=${cost_usd:.4}");
            }
            _ => {}
        }
    }

    let _ = handle.await;
}

// ── System Prompt ─────────────────────────────────────────────────────────────

/// Tool-use + verbosity guidelines appended to every system prompt regardless
/// of role / repo state.
fn shared_tool_guidance() -> &'static str {
    "\n\nTool use guidelines (IMPORTANT — follow to minimise token cost):\
     \n- To modify an existing file use edit_file (str_replace). Never read the whole file just to rewrite it.\
     \n- Use read_file with offset+limit to read only the section you need.\
     \n- Use grep to locate the exact lines before reading or editing.\
     \n- Use write_file only for creating new files.\
     \n- NEVER use a leading '**' glob in any path argument (e.g. **/dir/file). Always anchor paths from a known root.\
     \n- Be concise and precise.\
     \n- Only run git commit when the user explicitly instructs you to.\
     \n\nResponse style: answers should be concise but informative — get to the point without unnecessary padding, but include all details that are genuinely useful.\
     \n\nVerbosity rules (CRITICAL):\
     \n- Do NOT narrate tool calls. Never say what you are about to do before calling a tool.\
     \n- Do NOT summarise tool results in prose after they return. Let the results speak for themselves.\
     \n- Only write prose when you have a direct answer or question for the user.\
     \n- Never use filler phrases like \"I'll now...\", \"Let me...\", \"I've completed...\", \"Sure!\" etc.\
     \n- Never pad responses."
}

/// Background-command tool note shared by every role that exposes
/// `run_command_in_background`.
fn background_task_note() -> &'static str {
    "\n\nYou have a run_command_in_background(command) tool. Use it to run a shell command \
     that would otherwise block the current chat turn — long builds, big test suites, large \
     downloads. The command is executed with `bash -c` and its stdout/stderr is captured. \
     When it completes, the output is injected into this conversation as a 'Background \
     command … completed' message and you'll be invoked autonomously to react. If no \
     follow-up action is genuinely useful, reply with one short acknowledgement line rather \
     than producing prose; only continue working if the result clearly demands it. Do not \
     use this tool for fast commands — prefer the regular `bash` tool.\n\n\
     You also have a monitor_process tool for when you need to react to a process \
     *while it runs* rather than only at the end. Give it a `command` to start and watch a \
     new process, or a `task_id` to attach to a background task you already started. It \
     wakes you with new output at most every `wake_interval_secs` — pick that interval to \
     suit the process. The same 'react only if warranted, otherwise acknowledge briefly' \
     guidance applies to each wake-up."
}

pub fn build_system_prompt(repo_path: &str) -> String {
    let claude_md = match std::fs::read_to_string(format!("{}/CLAUDE.md", repo_path)) {
        Ok(s) => {
            debug!("[core] including CLAUDE.md ({} chars) from {repo_path}", s.len());
            format!("\n\n# Project instructions (CLAUDE.md)\n{}", s)
        }
        Err(_) => {
            debug!("[core] no CLAUDE.md at {repo_path}");
            String::new()
        }
    };

    let tool_guidance    = shared_tool_guidance();
    let bg_task_note      = background_task_note();
    let spawn_note        = spawn_capability_note();

    format!(
        "You are an AI assistant helping manage the git repository at {repo_path}.\
         You can inspect code, answer questions, and help coordinate work across branches.\
         Any path preceded by '@' (e.g. @src/main.rs) is a reference to a file path in the git repository.{claude_md}{tool_guidance}{bg_task_note}{spawn_note}"
    )
}

/// System prompt for a child container running as a general-purpose agent
/// (no git repo bound to the workspace). Operators can set `AGENT_PURPOSE`
/// to give the agent a specific mission; otherwise it boots with a generic
/// description and waits for instructions.
pub fn build_agent_system_prompt(workspace: &str) -> String {
    let purpose = std::env::var("AGENT_PURPOSE")
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    let purpose_block = match purpose {
        Some(p) => format!("\n\n# Purpose\n{p}"),
        None    => String::new(),
    };
    let tool_guidance    = shared_tool_guidance();
    let bg_task_note      = background_task_note();
    let spawn_note        = spawn_capability_note();

    format!(
        "You are an AI agent running in a containerized workspace at {workspace}.\
         You have bash, file-system, and (when configured) MCP-server tools available.\
         You are not bound to any specific git repository — treat the workspace as scratch \
         space unless the user gives you something else to work on.{purpose_block}{tool_guidance}{bg_task_note}{spawn_note}"
    )
}

/// Appended to the agent system prompt only when lair handed this child a
/// capability token (i.e. it was itself spawned by another agent). Operator-
/// spawned top-level agents don't see this section — they don't have the
/// tools either.
fn spawn_capability_note() -> String {
    if std::env::var("OCTO_AGENT_TOKEN").ok().filter(|s| !s.is_empty()).is_none() {
        return String::new();
    }
    "\n\n# Sub-agent orchestration\n\
     You can spawn your own child agents with `spawn_agent` and terminate any \
     agent you (transitively) spawned with `terminate_agent`. Children you \
     spawn are owned by you: if you are terminated, lair cascade-terminates \
     them too. There are operator-imposed caps on tree depth and total \
     descendants — if a spawn is refused, accept the cap rather than \
     retrying. Use sub-agents when a task genuinely benefits from isolated \
     state (a separate repo clone, a long-running build) — not as a \
     general parallelism mechanism."
        .to_string()
}

// ── Git ───────────────────────────────────────────────────────────────────────

pub fn get_branches_for_repo(repo: &str) -> Result<Vec<Branch>, String> {
    if repo.is_empty() { return Ok(vec![]); }
    let repo_obj = git2::Repository::open(repo).map_err(|e| {
        warn!("[git] failed to open repo {repo}: {e}");
        e.to_string()
    })?;
    let mut branches = Vec::new();
    let iter = repo_obj.branches(Some(git2::BranchType::Local)).map_err(|e| {
        warn!("[git] failed to list branches for {repo}: {e}");
        e.to_string()
    })?;
    for item in iter {
        let (b, _) = item.map_err(|e| e.to_string())?;
        let name   = b.name().ok().flatten().unwrap_or("").to_string();
        let commit = b.get().peel_to_commit()
            .map(|c| c.id().to_string()[..7].to_string())
            .unwrap_or_default();
        branches.push(Branch { name, commit });
    }
    debug!("[git] {repo}: {} local branch(es)", branches.len());
    Ok(branches)
}

// ── Shell Environment Bootstrap ───────────────────────────────────────────────

pub fn init_shell_env() {
    if std::env::var("OCTO_SKIP_SHELL_ENV").is_ok() {
        debug!("[core] OCTO_SKIP_SHELL_ENV set, skipping shell env init");
        return;
    }
    info!("[core] initializing shell environment from login shell");
    let output = std::process::Command::new("zsh")
        .args(["-l", "-c", "source ~/.zshrc 2>/dev/null; env -0"])
        .output()
        .or_else(|_| {
            std::process::Command::new("bash")
                .args(["-l", "-c", "source ~/.bashrc 2>/dev/null; env -0"])
                .output()
        });
    let Ok(output) = output else {
        warn!("[core] failed to run login shell for env init");
        return;
    };
    let Ok(env_str) = std::str::from_utf8(&output.stdout) else { return };
    let mut count = 0usize;
    for entry in env_str.split('\0') {
        if let Some((key, val)) = entry.split_once('=') {
            std::env::set_var(key, val);
            count += 1;
        }
    }
    debug!("[core] shell env init loaded {count} environment variables");
}
