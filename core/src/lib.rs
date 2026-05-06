use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};
use tokio_util::sync::CancellationToken;
use tracing::info;

pub mod mcp;
pub use mcp::{McpPool, init_mcp_pool, build_tools_with_mcp, chain_executor_with_mcp};

pub mod noise;
pub use noise::{
    DEV_PUBKEY_BASE32, DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC,
    load_or_generate_keypair, run_noise_proxy, to_base32,
};

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
use tokio::sync::{mpsc, oneshot};

// ── Data directory ────────────────────────────────────────────────────────────

/// Root data directory: $OCTO_DATA_DIR if set, otherwise $HOME/.octo.
/// In Docker this is set to /data so sessions survive image updates via a named volume.
pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("OCTO_DATA_DIR") {
        PathBuf::from(d)
    } else {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".octo")
    }
}

// ── Config ────────────────────────────────────────────────────────────────────

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Config {
    pub repo:    Option<String>,
    pub name:    Option<String>,
    pub api_key: Option<String>,
    pub model:   Option<String>,
}

pub fn read_config() -> Config {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn effective_repo(cfg: &Config) -> String {
    cfg.repo.clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        })
}

pub fn write_config(cfg: &Config) {
    let path = config_path();
    fs::create_dir_all(path.parent().unwrap()).ok();
    fs::write(path, serde_json::to_string(cfg).unwrap()).ok();
}

pub fn resolve_api_key() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty())
        .or_else(|| read_config().api_key)
        .or_else(|| read_key_from_shell_files())
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

// ── Chat Events ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    Ready              { session_id: String, resumed: bool },
    Text               { text: String },
    ToolUse            { tool: String, input: serde_json::Value },
    ToolOutput         { line: String },
    ToolResult         { tool_use_id: String, content: serde_json::Value },
    Result             { cost_usd: f64, turns: usize, session_id: String, result: Option<String> },
    Error              { message: String },
    Interrupted        { cost_usd: f64 },
    InterruptAck,
    Question           { question: String, #[serde(skip)] answer_tx: Option<oneshot::Sender<String>> },
    System             { text: String },
    Spawning           { task: String },
    WorkerCreated      { branch: String, worktree_path: String, task: String },
    WorkerError        { message: String },
    WorkerSessionReady { branch: String, worktree_path: String, worker_session_id: String, task: String },
}

// ── Branch ────────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct Branch {
    pub name:     String,
    pub commit:   String,
    pub worktree: Option<String>,
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

pub fn slug(text: &str) -> String {
    let s: String = text.to_lowercase().chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let parts: Vec<&str> = s.split('-').filter(|p| !p.is_empty()).collect();
    parts.join("-").chars().take(40).collect()
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
            input_schema: serde_json::json!({ "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] }) },
        AnthropicTool { name: "read_file".into(),
            description: "Read a file. Always use offset+limit to read only the section you need — never read the whole file if you already know the relevant line numbers from grep. offset is 0-based (first line = 0). Lines are returned with 1-based line numbers.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" }, "offset": { "type": "integer", "description": "0-based line index to start reading from" }, "limit": { "type": "integer", "description": "number of lines to return" } }, "required": ["path"] }) },
        AnthropicTool { name: "edit_file".into(),
            description: "Replace an exact string in a file. PREFER this over write_file for modifying existing files. old_str must match exactly once.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" }, "old_str": { "type": "string" }, "new_str": { "type": "string" } }, "required": ["path", "old_str", "new_str"] }) },
        AnthropicTool { name: "write_file".into(),
            description: "Write a file. Use for creating new files only; prefer edit_file for existing files.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" }, "content": { "type": "string" } }, "required": ["path", "content"] }) },
        AnthropicTool { name: "glob".into(),
            description: "Find files matching a glob pattern (e.g. src/**/*.rs).".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "pattern": { "type": "string" } }, "required": ["pattern"] }) },
        AnthropicTool { name: "grep".into(),
            description: "Search file contents for a regex pattern. Returns matching lines with file:line numbers. Use context to include surrounding lines. Pass the returned line numbers to read_file offset+limit to read more of that section.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "pattern": { "type": "string" }, "path": { "type": "string" }, "context": { "type": "integer", "description": "number of lines to show before and after each match (like grep -C)" } }, "required": ["pattern"] }) },
        AnthropicTool { name: "ask_user".into(),
            description: "Pause and ask the user a clarifying question. Returns the user's answer.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "question": { "type": "string" } }, "required": ["question"] }) },
        AnthropicTool { name: "web_fetch".into(),
            description: "Fetch a URL and return its text content (HTML stripped). Truncated at 50 000 chars.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"] }) },
    ];
    if std::env::var("BRAVE_API_KEY").ok().filter(|s| !s.is_empty()).is_some() {
        tools.push(AnthropicTool { name: "web_search".into(),
            description: "Search the web via Brave Search.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "query": { "type": "string" } }, "required": ["query"] }) });
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
                Err(e) => format!("error: {e}"),
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
                Err(e) => format!("error: {e}"),
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
                Err(e) => format!("error reading file: {e}"),
                Ok(content) => {
                    let count = content.matches(old_str).count();
                    if count == 0 {
                        "error: old_str not found in file".to_string()
                    } else if count > 1 {
                        format!("error: old_str matches {count} locations — make it more specific")
                    } else {
                        let updated = content.replacen(old_str, new_str, 1);
                        match fs::write(&full, updated) {
                            Ok(_)  => "ok".to_string(),
                            Err(e) => format!("error writing file: {e}"),
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
                Ok(_)  => "ok".to_string(),
                Err(e) => format!("error: {e}"),
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
                Err(e) => format!("error: {e}"),
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
                Err(e) => format!("error: {e}"),
            }
        }
        "ask_user" => {
            let question = input["question"].as_str().unwrap_or("").to_string();
            if question.is_empty() { return "error: question is required".to_string(); }
            let (otx, orx) = oneshot::channel::<String>();
            tx.send(ChatEvent::Question { question, answer_tx: Some(otx) }).await.ok();
            tokio::select! {
                res = orx => match res {
                    Ok(answer) => answer,
                    Err(_)     => "error: question was cancelled".to_string(),
                },
                _ = cancel.cancelled() => "error: interrupted".to_string(),
            }
        }
        "web_fetch" => {
            let url = input["url"].as_str().unwrap_or("");
            if url.is_empty() { return "error: url is required".to_string(); }
            match http_client().get(url)
                .header("User-Agent", "Mozilla/5.0 (compatible; octo/1.0)")
                .send().await {
                Err(e)   => format!("error: {e}"),
                Ok(resp) => {
                    let status = resp.status();
                    match resp.text().await {
                        Err(e)   => format!("error reading response: {e}"),
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
                None    => return "error: BRAVE_API_KEY environment variable not set".to_string(),
            };
            match http_client()
                .get("https://api.search.brave.com/res/v1/web/search")
                .query(&[("q", query), ("count", "10")])
                .header("Accept", "application/json")
                .header("X-Subscription-Token", api_key)
                .send().await
            {
                Err(e)   => format!("error: {e}"),
                Ok(resp) => match resp.json::<serde_json::Value>().await {
                    Err(e) => format!("error parsing response: {e}"),
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
            None    => format!("unknown tool: {name}"),
        },
    }
}

// ── Anthropic Streaming ───────────────────────────────────────────────────────

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

pub async fn call_turn(
    messages:    &[ApiMessage],
    system:      &str,
    model:       &str,
    api_key:     &str,
    cancel:      &CancellationToken,
    tx:          &mpsc::Sender<ChatEvent>,
    extra_tools: &[AnthropicTool],
) -> Result<(Vec<ContentBlock>, String, StreamUsage), String> {
    let mut tools: Vec<serde_json::Value> = tool_definitions_with_mcp(extra_tools)
        .into_iter().map(|t| serde_json::to_value(t).unwrap()).collect();
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = serde_json::json!({"type": "ephemeral"});
    }

    let compacted = compact_history(messages, 20);

    let mut messages_json: Vec<serde_json::Value> = compacted
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect();

    // Distribute up to 2 cache breakpoints across the message history.
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
        "model": model,
        "max_tokens": 8192,
        "system": [{"type":"text","text":system,"cache_control":{"type":"ephemeral"}}],
        "tools": tools,
        "messages": messages_json,
    });

    let response = tokio::select! {
        res = http_client()
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "prompt-caching-2024-07-31")
            .header("content-type", "application/json")
            .json(&body).send() => res.map_err(|e| e.to_string())?,
        _ = cancel.cancelled() => return Err("__interrupted__".to_string()),
    };

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("API error {status}: {text}"));
    }

    if cancel.is_cancelled() {
        return Err("__interrupted__".to_string());
    }

    let json: serde_json::Value = tokio::select! {
        res = response.json() => res.map_err(|e| e.to_string())?,
        _ = cancel.cancelled() => return Err("__interrupted__".to_string()),
    };

    let stop_reason = json["stop_reason"].as_str().unwrap_or("end_turn").to_string();
    let usage = &json["usage"];
    let stream_usage = StreamUsage {
        input_tokens:                usage["input_tokens"].as_u64().unwrap_or(0),
        output_tokens:               usage["output_tokens"].as_u64().unwrap_or(0),
        cache_creation_input_tokens: usage["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        cache_read_input_tokens:     usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
    };

    let mut blocks = Vec::new();
    if let Some(content) = json["content"].as_array() {
        for block in content {
            match block["type"].as_str().unwrap_or("") {
                "text" => {
                    let text = block["text"].as_str().unwrap_or("").to_string();
                    if !text.is_empty() {
                        tx.send(ChatEvent::Text { text: text.clone() }).await.ok();
                        blocks.push(ContentBlock::Text { text });
                    }
                }
                "tool_use" => {
                    let id    = block["id"].as_str().unwrap_or("").to_string();
                    let name  = block["name"].as_str().unwrap_or("").to_string();
                    let input = block["input"].clone();
                    tx.send(ChatEvent::ToolUse { tool: name.clone(), input: input.clone() }).await.ok();
                    blocks.push(ContentBlock::ToolUse { id, name, input });
                }
                _ => {}
            }
        }
    }

    Ok((blocks, stop_reason, stream_usage))
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
) -> Result<(String, f64, Vec<ApiMessage>), String> {
    let mut total_cost = 0.0f64;
    let mut last_text  = String::new();

    let (dummy_tx, _) = mpsc::channel::<ChatEvent>(1);
    let tx = event_tx.unwrap_or(dummy_tx);

    let tools: Vec<serde_json::Value> = {
        let mut t: Vec<serde_json::Value> = tool_definitions_with_mcp(extra_tools)
            .into_iter().map(|t| serde_json::to_value(t).unwrap()).collect();
        if let Some(last) = t.last_mut() {
            last["cache_control"] = serde_json::json!({"type": "ephemeral"});
        }
        t
    };

    let mut turn = 0usize;
    loop {
        turn += 1;
        if cancel.is_cancelled() {
            tx.send(ChatEvent::InterruptAck).await.ok();
            return Ok((last_text, total_cost, messages));
        }
        let compacted = compact_history(&messages, 20);
        let mut messages_json: Vec<serde_json::Value> = compacted.iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();

        // Prompt-cache breakpoints.
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
            "system":     [{"type":"text","text":system,"cache_control":{"type":"ephemeral"}}],
            "tools":      tools,
            "messages":   messages_json,
        });

        let response = tokio::select! {
            res = http_client()
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key",         api_key)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta",    "prompt-caching-2024-07-31")
                .header("content-type",      "application/json")
                .json(&body).send() => res.map_err(|e| e.to_string())?,
            _ = cancel.cancelled() => {
                tx.send(ChatEvent::InterruptAck).await.ok();
                return Ok((last_text, total_cost, messages));
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let text   = response.text().await.unwrap_or_default();
            return Err(format!("API error {status}: {text}"));
        }

        let json: serde_json::Value = tokio::select! {
            res = response.json() => res.map_err(|e| e.to_string())?,
            _ = cancel.cancelled() => {
                tx.send(ChatEvent::InterruptAck).await.ok();
                return Ok((last_text, total_cost, messages));
            }
        };

        let stop_reason = json["stop_reason"].as_str().unwrap_or("end_turn").to_string();
        let usage = &json["usage"];
        info!(
            "[send_message] turn={turn} stop_reason={stop_reason} in={} out={} cache_create={} cache_read={}",
            usage["input_tokens"].as_u64().unwrap_or(0),
            usage["output_tokens"].as_u64().unwrap_or(0),
            usage["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
        );
        total_cost += cost_usd(
            model,
            usage["input_tokens"].as_u64().unwrap_or(0),
            usage["output_tokens"].as_u64().unwrap_or(0),
            usage["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
        );

        let mut text_buf  = String::new();
        let mut blocks    = Vec::new();
        let mut tool_uses: Vec<(String, String, serde_json::Value)> = Vec::new();

        if let Some(content) = json["content"].as_array() {
            for block in content {
                match block["type"].as_str().unwrap_or("") {
                    "text" => {
                        let t = block["text"].as_str().unwrap_or("").to_string();
                        if !t.is_empty() {
                            let preview = t.chars().take(120).collect::<String>();
                            info!("[send_message] turn={turn} text ({} chars): {preview}", t.len());
                            tx.send(ChatEvent::Text { text: t.clone() }).await.ok();
                            text_buf.push_str(&t);
                            blocks.push(ContentBlock::Text { text: t });
                        }
                    }
                    "tool_use" => {
                        let id    = block["id"].as_str().unwrap_or("").to_string();
                        let name  = block["name"].as_str().unwrap_or("").to_string();
                        let input = block["input"].clone();
                        info!("[send_message] turn={turn} tool_use name={name} input={input}");
                        tx.send(ChatEvent::ToolUse { tool: name.clone(), input: input.clone() }).await.ok();
                        tool_uses.push((id.clone(), name.clone(), input.clone()));
                        blocks.push(ContentBlock::ToolUse { id, name, input });
                    }
                    _ => {}
                }
            }
        }

        messages.push(ApiMessage { role: "assistant".to_string(), content: blocks });

        if stop_reason != "tool_use" || tool_uses.is_empty() {
            return Ok((text_buf, total_cost, messages));
        }

        last_text = text_buf;

        // Execute tools and collect results.
        let mut results = Vec::new();
        let mut ack_sent = false;
        for (id, name, input) in tool_uses {
            if cancel.is_cancelled() {
                if !ack_sent {
                    tx.send(ChatEvent::InterruptAck).await.ok();
                    ack_sent = true;
                }
                // Append a synthetic tool_result so history stays valid.
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
    let mut turns                        = 0usize;
    let mut total_input                  = 0u64;
    let mut total_output                 = 0u64;
    let mut total_cache_creation_input   = 0u64;
    let mut total_cache_read_input       = 0u64;

    const MAX_TURNS: usize = 100;

    loop {
        if turns >= MAX_TURNS {
            tx.send(ChatEvent::Error {
                message: format!("Stopped after {MAX_TURNS} turns to prevent runaway loop"),
            }).await.ok();
            return;
        }

        let (messages, system, cwd, cancel) = {
            let s = session.lock().unwrap();
            (s.messages.clone(), s.system_prompt.clone(), s.cwd.clone(), s.cancel.clone())
        };

        if cancel.is_cancelled() {
            let partial_cost = cost_usd(&model, total_input, total_output, total_cache_creation_input, total_cache_read_input);
            tx.send(ChatEvent::InterruptAck).await.ok();
            tx.send(ChatEvent::Interrupted { cost_usd: partial_cost }).await.ok();
            return;
        }

        match call_turn(&messages, &system, &model, &api_key, &cancel, &tx, &extra_tools).await {
            Err(e) if e == "__interrupted__" => {
                let partial_cost = cost_usd(&model, total_input, total_output, total_cache_creation_input, total_cache_read_input);
                tx.send(ChatEvent::InterruptAck).await.ok();
                tx.send(ChatEvent::Interrupted { cost_usd: partial_cost }).await.ok();
                return;
            }
            Err(e) => {
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
                    let cost = cost_usd(&model, total_input, total_output, total_cache_creation_input, total_cache_read_input);
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
                    let partial_cost = cost_usd(&model, total_input, total_output, total_cache_creation_input, total_cache_read_input);
                    tx.send(ChatEvent::Interrupted { cost_usd: partial_cost }).await.ok();
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
            ChatEvent::ToolUse { tool, input } => {
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

/// System prompt for the ephemeral loop that handles inbound message_lair /
/// message_child calls.  The loop runs to completion and its final text is
/// returned as the HTTP response — so the model MUST always emit a text block
/// in the last turn.
pub fn build_ephemeral_system_prompt() -> &'static str {
    "You are responding to a query from another container in the octo network. \
     Use whatever tools are available to answer fully. \
     IMPORTANT: you MUST end your response with a text message that directly answers \
     the query — even if you used tools to gather information, always write a final \
     text summary before stopping. Never end on a tool_use turn."
}

pub fn build_system_prompt(repo_path: &str, branch: Option<&str>, worktree_path: Option<&str>) -> String {
    let tool_guidance = "\n\nTool use guidelines (IMPORTANT — follow to minimise token cost):\
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
        \n- Never pad responses.";

    let parent_tool_note = if std::env::var("LAIR_URL").is_ok() {
        "\n\nYou have a message_lair(text) tool available. Use it to send a message to the parent \
         (lair) container's agent and receive a response — for example to request secrets, \
         configuration, or to hand off a task."
    } else {
        ""
    };

    let claude_md = std::fs::read_to_string(format!("{}/CLAUDE.md", repo_path))
        .map(|s| format!("\n\n# Project instructions (CLAUDE.md)\n{}", s))
        .unwrap_or_default();

    match (branch, worktree_path) {
        (Some(branch), Some(wt)) => format!(
            "You are an AI coding assistant working on branch '{branch}' in the git worktree at {wt}.\
             This is your working directory — use it for all file operations and git commands.\
             Do not cd to any other directory.\
             Any path preceded by '@' (e.g. @src/main.rs) is a reference to a file path in the git repository.{claude_md}{tool_guidance}{parent_tool_note}"
        ),
        _ => format!(
            "You are an AI assistant helping manage the git repository at {repo_path}.\
             You can inspect code, answer questions, and help coordinate work across branches.\
             Any path preceded by '@' (e.g. @src/main.rs) is a reference to a file path in the git repository.{claude_md}{tool_guidance}{parent_tool_note}"
        ),
    }
}

// ── Git ───────────────────────────────────────────────────────────────────────

pub fn get_branches_for_repo(repo: &str) -> Result<Vec<Branch>, String> {
    if repo.is_empty() { return Ok(vec![]); }
    let repo_obj = git2::Repository::open(repo).map_err(|e| e.to_string())?;

    let mut worktree_map: HashMap<String, String> = HashMap::new();
    if let Ok(names) = repo_obj.worktrees() {
        for wt_name in names.iter().flatten() {
            if let Ok(wt) = repo_obj.find_worktree(wt_name) {
                let path = wt.path();
                if let Ok(wt_repo) = git2::Repository::open(path) {
                    if let Ok(head) = wt_repo.head() {
                        if let Some(short) = head.shorthand() {
                            worktree_map.insert(short.to_string(), path.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }
    }

    let mut branches = Vec::new();
    let iter = repo_obj.branches(Some(git2::BranchType::Local)).map_err(|e| e.to_string())?;
    for item in iter {
        let (b, _) = item.map_err(|e| e.to_string())?;
        let name   = b.name().ok().flatten().unwrap_or("").to_string();
        let commit = b.get().peel_to_commit()
            .map(|c| c.id().to_string()[..7].to_string())
            .unwrap_or_default();
        let worktree = worktree_map.get(&name).cloned();
        branches.push(Branch { name, commit, worktree });
    }
    Ok(branches)
}

pub async fn generate_branch_name(task: &str, api_key: &str) -> String {
    let body = serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 32,
        "messages": [{ "role": "user", "content": format!(
            "Generate a short git branch name (2-4 words, lowercase, hyphenated, no punctuation) \
             for this task: {task}\n\nReply with only the branch name, nothing else."
        )}]
    });
    let name: String = async {
        let resp = http_client()
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body).send().await.ok()?;
        if !resp.status().is_success() { return None; }
        let v: serde_json::Value = resp.json().await.ok()?;
        let text = v["content"][0]["text"].as_str()?.trim().to_string();
        Some(text)
    }.await.unwrap_or_default();
    let cleaned = slug(&name);
    if cleaned.is_empty() { slug(task) } else { cleaned }
}

pub fn create_worktree(repo_path: &str, branch: &str) -> Result<String, String> {
    let repo_name = PathBuf::from(repo_path).file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let worktree_path = data_dir().join("worktrees").join(&repo_name).join(branch);
    if let Some(parent) = worktree_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let out = std::process::Command::new("git")
        .args(["worktree", "add", "-b", branch, &worktree_path.to_string_lossy(), "HEAD"])
        .current_dir(repo_path).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(worktree_path.to_string_lossy().to_string())
}

// ── Shell Environment Bootstrap ───────────────────────────────────────────────

pub fn init_shell_env() {
    if std::env::var("OCTO_SKIP_SHELL_ENV").is_ok() {
        return;
    }
    let output = std::process::Command::new("zsh")
        .args(["-l", "-c", "source ~/.zshrc 2>/dev/null; env -0"])
        .output()
        .or_else(|_| {
            std::process::Command::new("bash")
                .args(["-l", "-c", "source ~/.bashrc 2>/dev/null; env -0"])
                .output()
        });
    let Ok(output) = output else { return };
    let Ok(env_str) = std::str::from_utf8(&output.stdout) else { return };
    for entry in env_str.split('\0') {
        if let Some((key, val)) = entry.split_once('=') {
            std::env::set_var(key, val);
        }
    }
}
