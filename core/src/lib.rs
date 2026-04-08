use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
};

pub mod mcp;
pub use mcp::{McpPool, init_mcp_pool};

// ── Shared HTTP client ────────────────────────────────────────────────────────

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(reqwest::Client::new)
}

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

// ── Data directory ────────────────────────────────────────────────────────────

/// Root data directory: $CLAUDULHU_DATA_DIR if set, otherwise $HOME/.claudulhu.
/// In Docker this is set to /data so sessions survive image updates via a named volume.
pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDULHU_DATA_DIR") {
        PathBuf::from(d)
    } else {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claudulhu")
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
    pub messages:         Vec<ApiMessage>,
    pub system_prompt:    String,
    pub cwd:              String,
    pub aborted:          Arc<AtomicBool>,
    pub pending_question: Arc<tokio::sync::Mutex<Option<oneshot::Sender<String>>>>,
    /// Connected MCP server clients.  Populated once at session creation and
    /// shared across all turns in the agentic loop.
    pub mcp_pool:         McpPool,
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

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    Ready              { session_id: String, resumed: bool },
    Text               { text: String },
    ToolUse            { tool: String, input: serde_json::Value },
    ToolResult         { tool_use_id: String, content: serde_json::Value },
    Result             { cost_usd: f64, turns: usize, session_id: String, result: Option<String> },
    Error              { message: String },
    Interrupted        { cost_usd: f64 },
    Question           { question: String },
    System             { text: String },
    Spawning           { task: String },
    WorkerCreated      { branch: String, worktree_path: String, task: String },
    WorkerError        { message: String },
    WorkerSessionReady { branch: String, worktree_path: String, worker_session_id: String, task: String },
    /// Model is beginning a multi-step agentic session.
    SessionStart       { label: String, session_id: String },
    /// Model is ending an agentic session; summary is the final prose response.
    SessionEnd         { summary: String },
}

// ── Branch ────────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct Branch {
    pub name:     String,
    pub commit:   String,
    pub worktree: Option<String>,
}

// ── Task Management ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Task {
    pub id:          String,
    pub subject:     String,
    pub description: String,
    pub active_form: Option<String>,
    pub status:      String,
    pub owner:       Option<String>,
    pub output:      Option<String>,
    pub blocks:      Vec<String>,
    pub blocked_by:  Vec<String>,
    pub created_at:  u64,
    pub updated_at:  u64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct TaskStore {
    pub next_id: u32,
    pub tasks:   Vec<Task>,
}

pub fn tasks_path() -> PathBuf {
    data_dir().join("tasks.json")
}

pub fn read_task_store() -> TaskStore {
    fs::read_to_string(tasks_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn write_task_store(store: &TaskStore) {
    let path = tasks_path();
    fs::create_dir_all(path.parent().unwrap()).ok();
    fs::write(path, serde_json::to_string_pretty(store).unwrap()).ok();
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
        // Task output is subprocess/agent output — can be substantial.
        "task_output" =>  6_000,
        // Search results: 10 results × ~400 chars each saturates well under 4 k.
        "web_search"  =>  4_000,
        // Match lists: more than ~6 k of grep hits is noise the model won't act on.
        "grep"        =>  4_000,
        // Task records are short structured JSON.
        "task_get"    =>  2_000,
        // Task lists and file-path lists are inherently short.
        "task_list"   =>  3_000,
        "glob"        =>  3_000,
        // Everything else (edit_file, write_file, task_create, task_update,
        // task_stop, ask_user, create_pull_request) returns a short fixed string;
        // 2 000 is a safe ceiling that costs nothing in practice.
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
    tools.extend_from_slice(extra);
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
        AnthropicTool { name: "task_create".into(),
            description: "Create a task with status 'pending'. Returns the task ID.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "subject": { "type": "string" }, "description": { "type": "string" }, "activeForm": { "type": "string" } }, "required": ["subject", "description"] }) },
        AnthropicTool { name: "task_list".into(),
            description: "List all non-deleted tasks.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": {} }) },
        AnthropicTool { name: "task_get".into(),
            description: "Get full details of a task by ID.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "taskId": { "type": "string" } }, "required": ["taskId"] }) },
        AnthropicTool { name: "task_update".into(),
            description: "Update a task's status, subject, description, owner, or dependencies.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": {
                "taskId":       { "type": "string" },
                "status":       { "type": "string" },
                "subject":      { "type": "string" },
                "description":  { "type": "string" },
                "activeForm":   { "type": "string" },
                "owner":        { "type": "string" },
                "addBlocks":    { "type": "array", "items": { "type": "string" } },
                "addBlockedBy": { "type": "array", "items": { "type": "string" } }
            }, "required": ["taskId"] }) },
        AnthropicTool { name: "task_stop".into(),
            description: "Cancel (delete) a task by ID.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "task_id": { "type": "string" } }, "required": ["task_id"] }) },
        AnthropicTool { name: "task_output".into(),
            description: "Get the output field of a task.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "task_id": { "type": "string" } }, "required": ["task_id"] }) },
        AnthropicTool { name: "web_fetch".into(),
            description: "Fetch a URL and return its text content (HTML stripped). Truncated at 50 000 chars.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"] }) },
        AnthropicTool { name: "create_pull_request".into(),
            description: "Create a pull request (GitHub) or merge request (GitLab) from the current branch. Requires GH_TOKEN env var. Detects the host from the repo's git remote URL. Use after pushing a branch to propose merging it into the base branch.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": {
                "title": { "type": "string", "description": "PR/MR title" },
                "body":  { "type": "string", "description": "PR/MR description (markdown)" },
                "head":  { "type": "string", "description": "Source branch to merge from (defaults to current branch)" },
                "base":  { "type": "string", "description": "Target branch to merge into (defaults to main)" }
            }, "required": ["title"] }) },
    ];
    tools.push(AnthropicTool { name: "session_start".into(),
        description: "MUST be called before any other tool. Required whenever any tool use is needed — even a single tool call. Provide a short label describing what you are about to do. Do NOT call this for simple questions or responses that require no tools.".into(),
        input_schema: serde_json::json!({ "type": "object", "properties": { "label": { "type": "string", "description": "Short description of the work being done, e.g. \"refactoring the auth module\"" } }, "required": ["label"] }) });
    tools.push(AnthropicTool { name: "session_end".into(),
        description: "MUST be called after all other tools, as the final tool call. Required whenever session_start was called. Provide a concise summary of what was done and the outcome — this is shown to the user as the response.".into(),
        input_schema: serde_json::json!({ "type": "object", "properties": { "summary": { "type": "string", "description": "Concise summary of what was done and the outcome." } }, "required": ["summary"] }) });
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
    pending_question: Arc<tokio::sync::Mutex<Option<oneshot::Sender<String>>>>,
    mcp_pool:         &McpPool,
) -> String {
    match name {
        "bash" => {
            let cmd = input["command"].as_str().unwrap_or("");
            match tokio::process::Command::new("bash")
                .arg("-c").arg(cmd)
                .current_dir(cwd)
                .output().await
            {
                Ok(o) => {
                    let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                    if stderr.is_empty() { stdout } else { format!("{stdout}\n[stderr]: {stderr}") }
                }
                Err(e) => format!("error: {e}"),
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
            { *pending_question.lock().await = Some(otx); }
            tx.send(ChatEvent::Question { question }).await.ok();
            match orx.await {
                Ok(answer) => answer,
                Err(_)     => "error: question was cancelled".to_string(),
            }
        }
        "task_create" => {
            let subject = input["subject"].as_str().unwrap_or("").to_string();
            if subject.is_empty() { return "error: subject is required".to_string(); }
            let description = input["description"].as_str().unwrap_or("").to_string();
            let active_form = input["activeForm"].as_str().map(|s| s.to_string());
            let now = now_secs();
            let mut store = read_task_store();
            store.next_id += 1;
            let id = store.next_id.to_string();
            store.tasks.push(Task {
                id: id.clone(), subject, description, active_form,
                status: "pending".to_string(), owner: None, output: None,
                blocks: vec![], blocked_by: vec![], created_at: now, updated_at: now,
            });
            write_task_store(&store);
            format!("created task {id}")
        }
        "task_list" => {
            let store = read_task_store();
            let visible: Vec<&Task> = store.tasks.iter().filter(|t| t.status != "deleted").collect();
            if visible.is_empty() { return "no tasks".to_string(); }
            visible.iter().map(|t| {
                let blocked = if t.blocked_by.is_empty() { String::new() }
                              else { format!(" [blocked by: {}]", t.blocked_by.join(", ")) };
                let owner = t.owner.as_deref().map(|o| format!(" owner={o}")).unwrap_or_default();
                format!("[{}] {} — {}{}{}", t.id, t.status, t.subject, owner, blocked)
            }).collect::<Vec<_>>().join("\n")
        }
        "task_get" => {
            let id = input["taskId"].as_str().unwrap_or("");
            let store = read_task_store();
            match store.tasks.iter().find(|t| t.id == id) {
                None    => format!("error: task {id} not found"),
                Some(t) => serde_json::to_string_pretty(t).unwrap_or_default(),
            }
        }
        "task_update" => {
            let id = input["taskId"].as_str().unwrap_or("");
            let mut store = read_task_store();
            match store.tasks.iter_mut().find(|t| t.id == id) {
                None => format!("error: task {id} not found"),
                Some(t) => {
                    if let Some(s) = input["status"].as_str()      { t.status      = s.to_string(); }
                    if let Some(s) = input["subject"].as_str()     { t.subject     = s.to_string(); }
                    if let Some(s) = input["description"].as_str() { t.description = s.to_string(); }
                    if let Some(s) = input["activeForm"].as_str()  { t.active_form = Some(s.to_string()); }
                    if let Some(s) = input["owner"].as_str()       { t.owner       = Some(s.to_string()); }
                    if let Some(arr) = input["addBlocks"].as_array() {
                        for v in arr {
                            if let Some(s) = v.as_str() {
                                if !t.blocks.contains(&s.to_string()) { t.blocks.push(s.to_string()); }
                            }
                        }
                    }
                    if let Some(arr) = input["addBlockedBy"].as_array() {
                        for v in arr {
                            if let Some(s) = v.as_str() {
                                if !t.blocked_by.contains(&s.to_string()) { t.blocked_by.push(s.to_string()); }
                            }
                        }
                    }
                    t.updated_at = now_secs();
                    write_task_store(&store);
                    "ok".to_string()
                }
            }
        }
        "task_stop" => {
            let id = input["task_id"].as_str().or_else(|| input["taskId"].as_str()).unwrap_or("");
            let mut store = read_task_store();
            match store.tasks.iter_mut().find(|t| t.id == id) {
                None => format!("error: task {id} not found"),
                Some(t) => { t.status = "deleted".to_string(); t.updated_at = now_secs(); write_task_store(&store); "ok".to_string() }
            }
        }
        "task_output" => {
            let id = input["task_id"].as_str().unwrap_or("");
            let store = read_task_store();
            match store.tasks.iter().find(|t| t.id == id) {
                None    => format!("error: task {id} not found"),
                Some(t) => t.output.clone().unwrap_or_else(|| "(no output)".to_string()),
            }
        }
        "web_fetch" => {
            let url = input["url"].as_str().unwrap_or("");
            if url.is_empty() { return "error: url is required".to_string(); }
            match http_client().get(url)
                .header("User-Agent", "Mozilla/5.0 (compatible; claudulhu/1.0)")
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
        "create_pull_request" => {
            let token = match std::env::var("GH_TOKEN").ok().filter(|s| !s.is_empty()) {
                Some(t) => t,
                None    => return "error: GH_TOKEN environment variable not set".to_string(),
            };
            let title = input["title"].as_str().unwrap_or("").to_string();
            if title.is_empty() { return "error: title is required".to_string(); }
            let body  = input["body"].as_str().unwrap_or("").to_string();
            let base  = input["base"].as_str().unwrap_or("main").to_string();

            // Determine head branch
            let head = if let Some(h) = input["head"].as_str().filter(|s| !s.is_empty()) {
                h.to_string()
            } else {
                match tokio::process::Command::new("git")
                    .args(["rev-parse", "--abbrev-ref", "HEAD"])
                    .current_dir(cwd).output().await
                {
                    Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
                    Err(e) => return format!("error getting current branch: {e}"),
                }
            };

            // Get remote URL to detect host and parse owner/repo
            let remote_url = match tokio::process::Command::new("git")
                .args(["remote", "get-url", "origin"])
                .current_dir(cwd).output().await
            {
                Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
                Err(e) => return format!("error getting remote URL: {e}"),
            };

            // Parse owner/repo from https or ssh remote URLs
            // https://github.com/owner/repo.git  OR  git@github.com:owner/repo.git
            let path_part = if remote_url.starts_with("git@") {
                remote_url.splitn(2, ':').nth(1).unwrap_or("").to_string()
            } else {
                remote_url.trim_start_matches("https://")
                    .splitn(2, '/').skip(1).collect::<Vec<_>>().join("/")
            };
            let repo_path = path_part.trim_end_matches(".git").to_string();

            if remote_url.contains("github.com") {
                let url = format!("https://api.github.com/repos/{repo_path}/pulls");
                match http_client().post(&url)
                    .bearer_auth(&token)
                    .header("User-Agent", "claudulhu")
                    .header("Accept", "application/vnd.github+json")
                    .json(&serde_json::json!({ "title": title, "body": body, "head": head, "base": base }))
                    .send().await
                {
                    Err(e) => format!("error: {e}"),
                    Ok(resp) => {
                        let status = resp.status();
                        match resp.json::<serde_json::Value>().await {
                            Err(e) => format!("error parsing response: {e}"),
                            Ok(v) => if status.is_success() {
                                format!("Pull request created: {}", v["html_url"].as_str().unwrap_or(""))
                            } else {
                                format!("HTTP {status}: {}", v["message"].as_str().unwrap_or(&v.to_string()))
                            },
                        }
                    }
                }
            } else if remote_url.contains("gitlab.com") || remote_url.contains("gitlab.") {
                // GitLab: project path must be URL-encoded
                let encoded = repo_path.replace('/', "%2F");
                let url = format!("https://gitlab.com/api/v4/projects/{encoded}/merge_requests");
                match http_client().post(&url)
                    .header("PRIVATE-TOKEN", &token)
                    .json(&serde_json::json!({ "title": title, "description": body, "source_branch": head, "target_branch": base }))
                    .send().await
                {
                    Err(e) => format!("error: {e}"),
                    Ok(resp) => {
                        let status = resp.status();
                        match resp.json::<serde_json::Value>().await {
                            Err(e) => format!("error parsing response: {e}"),
                            Ok(v) => if status.is_success() {
                                format!("Merge request created: {}", v["web_url"].as_str().unwrap_or(""))
                            } else {
                                format!("HTTP {status}: {}", v["message"].as_str().unwrap_or(&v.to_string()))
                            },
                        }
                    }
                }
            } else {
                format!("error: unsupported git host in remote URL: {remote_url}")
            }
        }
        _ => {
            // Dispatch to MCP servers before giving up.
            if let Some(result) = mcp::pool_call_tool(mcp_pool, name, input.clone()).await {
                result
            } else {
                format!("unknown tool: {name}")
            }
        }
    }
}

// ── Anthropic Streaming ───────────────────────────────────────────────────────

pub struct StreamUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

enum PartialBlock {
    Text    { text: String },
    ToolUse { id: String, name: String, partial_json: String },
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

pub async fn stream_turn(
    messages:  &[ApiMessage],
    system:    &str,
    model:     &str,
    api_key:   &str,
    aborted:   &AtomicBool,
    tx:        &mpsc::Sender<ChatEvent>,
    mcp_pool:  &McpPool,
) -> Result<(Vec<ContentBlock>, String, StreamUsage), String> {
    let mcp_tools = mcp::pool_tool_definitions(mcp_pool).await;
    let mut tools: Vec<serde_json::Value> = tool_definitions_with_mcp(&mcp_tools)
        .into_iter().map(|t| serde_json::to_value(t).unwrap()).collect();
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = serde_json::json!({"type": "ephemeral"});
    }

    let compacted = compact_history(messages, 20);

    // Serialize messages to JSON so we can inject cache_control without
    // polluting the ContentBlock data model with API transport concerns.
    let mut messages_json: Vec<serde_json::Value> = compacted
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect();

    // Distribute up to 2 cache breakpoints across the message history.
    // The system prompt and tool list each consume one of Anthropic's 4
    // breakpoint limit, leaving 2 for message history.  Spreading them out
    // means later turns hit multiple cached prefixes instead of just the most
    // recent one, which cuts costs in long agentic sessions.
    //
    // We always anchor one breakpoint at the second-to-last message (the most
    // recent stable point before the current user input) and spread the
    // remaining one through the earlier history.
    let n = messages_json.len();
    if n >= 2 {
        // Candidate indices: evenly spaced, always including n-2.
        let candidates: Vec<usize> = if n < 4 {
            vec![n - 2]
        } else {
            // 2 breakpoints: full history cached at n-2; n/2 as TTL fallback.
            vec![n - 2, n / 2]
        };

        // Deduplicate and apply.
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
        "stream": true,
    });

    let response = http_client()
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "prompt-caching-2024-07-31")
        .header("content-type", "application/json")
        .json(&body).send().await.map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("API error {status}: {text}"));
    }

    let mut stream = response.bytes_stream();
    let mut buf    = String::new();
    let mut partial: HashMap<usize, PartialBlock> = HashMap::new();
    let mut completed: Vec<(usize, ContentBlock)> = Vec::new();
    let mut input_tokens:  u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut cache_creation_input_tokens: u64 = 0;
    let mut cache_read_input_tokens:     u64 = 0;
    let mut stop_reason = "end_turn".to_string();

    while let Some(chunk) = stream.next().await {
        if aborted.load(Ordering::Relaxed) {
            return Err("__interrupted__".to_string());
        }

        let bytes = chunk.map_err(|e| e.to_string())?;
        buf.push_str(&String::from_utf8_lossy(&bytes));

        loop {
            let Some(nl) = buf.find('\n') else { break };
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf = buf[nl + 1..].to_string();

            if !line.starts_with("data: ") { continue; }
            let json_str = &line[6..];
            if json_str == "[DONE]" { break; }

            let Ok(ev) = serde_json::from_str::<serde_json::Value>(json_str) else { continue; };

            match ev["type"].as_str().unwrap_or("") {
                "message_start" => {
                    let usage = &ev["message"]["usage"];
                    if let Some(u) = usage["input_tokens"].as_u64()                  { input_tokens = u; }
                    if let Some(u) = usage["cache_creation_input_tokens"].as_u64()   { cache_creation_input_tokens = u; }
                    if let Some(u) = usage["cache_read_input_tokens"].as_u64()        { cache_read_input_tokens = u; }
                }
                "content_block_start" => {
                    let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                    match ev["content_block"]["type"].as_str().unwrap_or("") {
                        "text"     => { partial.insert(idx, PartialBlock::Text { text: String::new() }); }
                        "tool_use" => {
                            let id   = ev["content_block"]["id"].as_str().unwrap_or("").to_string();
                            let name = ev["content_block"]["name"].as_str().unwrap_or("").to_string();
                            partial.insert(idx, PartialBlock::ToolUse { id, name, partial_json: String::new() });
                        }
                        _ => {}
                    }
                }
                "content_block_delta" => {
                    let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                    match ev["delta"]["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            let delta = ev["delta"]["text"].as_str().unwrap_or("");
                            if !delta.is_empty() {
                                if let Some(PartialBlock::Text { text }) = partial.get_mut(&idx) {
                                    text.push_str(delta);
                                }
                                tx.send(ChatEvent::Text { text: delta.to_string() }).await.ok();
                            }
                        }
                        "input_json_delta" => {
                            let delta = ev["delta"]["partial_json"].as_str().unwrap_or("");
                            if let Some(PartialBlock::ToolUse { partial_json, .. }) = partial.get_mut(&idx) {
                                partial_json.push_str(delta);
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                    if let Some(block) = partial.remove(&idx) {
                        match block {
                            PartialBlock::Text { text } => {
                                completed.push((idx, ContentBlock::Text { text }));
                            }
                            PartialBlock::ToolUse { id, name, partial_json } => {
                                let input: serde_json::Value = serde_json::from_str(&partial_json)
                                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                                tx.send(ChatEvent::ToolUse { tool: name.clone(), input: input.clone() }).await.ok();
                                completed.push((idx, ContentBlock::ToolUse { id, name, input }));
                            }
                        }
                    }
                }
                "message_delta" => {
                    if let Some(sr) = ev["delta"]["stop_reason"].as_str() {
                        stop_reason = sr.to_string();
                    }
                    if let Some(u) = ev["usage"]["output_tokens"].as_u64() {
                        output_tokens = u;
                    }
                }
                _ => {}
            }
        }
    }

    completed.sort_by_key(|(i, _)| *i);
    let blocks: Vec<ContentBlock> = completed.into_iter().map(|(_, b)| b).collect();
    Ok((blocks, stop_reason, StreamUsage { input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens }))
}

// ── Agentic Loop ──────────────────────────────────────────────────────────────

pub async fn run_agentic_loop(
    session:    Arc<Mutex<Session>>,
    session_id: String,
    api_key:    String,
    model:      String,
    tx:         mpsc::Sender<ChatEvent>,
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

        let (messages, system, cwd, aborted, pending_question, mcp_pool) = {
            let s = session.lock().unwrap();
            (s.messages.clone(), s.system_prompt.clone(), s.cwd.clone(), s.aborted.clone(), s.pending_question.clone(), s.mcp_pool.clone())
        };

        if aborted.load(Ordering::Relaxed) {
            let partial_cost = cost_usd(&model, total_input, total_output, total_cache_creation_input, total_cache_read_input);
            tx.send(ChatEvent::Interrupted { cost_usd: partial_cost }).await.ok();
            return;
        }

        match stream_turn(&messages, &system, &model, &api_key, &aborted, &tx, &mcp_pool).await {
            Err(e) if e == "__interrupted__" => {
                let partial_cost = cost_usd(&model, total_input, total_output, total_cache_creation_input, total_cache_read_input);
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
                        // session_start / session_end are client-side signals — emit events,
                        // return a synthetic ok result, but do not execute anything.
                        if name == "session_start" {
                            let label = input["label"].as_str().unwrap_or("working").to_string();
                            let session_id = uuid::Uuid::new_v4().to_string();
                            tx.send(ChatEvent::SessionStart { label, session_id }).await.ok();
                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![serde_json::json!({"type":"text","text":"ok"})],
                            });
                            continue;
                        }
                        if name == "session_end" {
                            let summary = input["summary"].as_str().unwrap_or("").to_string();
                            tx.send(ChatEvent::SessionEnd { summary }).await.ok();
                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![serde_json::json!({"type":"text","text":"ok"})],
                            });
                            continue;
                        }
                        let result = truncate_tool_output(
                            execute_tool(name, input, &cwd, &tx, pending_question.clone(), &mcp_pool).await,
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

                {
                    let mut s = session.lock().unwrap();
                    s.messages.push(ApiMessage { role: "user".to_string(), content: tool_results });
                }
            }
        }
    }
}

// ── System Prompt ─────────────────────────────────────────────────────────────

pub fn build_system_prompt(repo_path: &str, branch: Option<&str>, worktree_path: Option<&str>) -> String {
    let tool_guidance = "\n\nSession guidelines (CRITICAL):\
        \n- ANY use of tools — even a single tool call — MUST be wrapped: call session_start first, then the tool(s), then session_end last.\
        \n- session_start must be the very first tool call; no other tool may be called before it.\
        \n- session_end must be the very last tool call; no other tool may be called after it.\
        \n- For simple questions or conversational replies that require no tool use, answer directly — do NOT call session_start or session_end.\
        \n\nTool use guidelines (IMPORTANT — follow to minimise token cost):\
        \n- To modify an existing file use edit_file (str_replace). Never read the whole file just to rewrite it.\
        \n- Use read_file with offset+limit to read only the section you need.\
        \n- Use grep to locate the exact lines before reading or editing.\
        \n- Use write_file only for creating new files.\
        \n- Be concise and precise.\
        \n\nResponse style: answers should be concise but informative — get to the point without unnecessary padding, but include all details that are genuinely useful.\
        \n\nVerbosity rules (CRITICAL):\
        \n- Do NOT narrate tool calls. Never say what you are about to do before calling a tool.\
        \n- Do NOT summarise tool results in prose after they return. Let the results speak for themselves.\
        \n- Only write prose when you have a direct answer or question for the user.\
        \n- Never use filler phrases like \"I'll now...\", \"Let me...\", \"I've completed...\", \"Sure!\" etc.\
        \n- Never pad responses.";

    let claude_md = std::fs::read_to_string(format!("{}/CLAUDE.md", repo_path))
        .map(|s| format!("\n\n# Project instructions (CLAUDE.md)\n{}", s))
        .unwrap_or_default();

    match (branch, worktree_path) {
        (Some(branch), Some(wt)) => format!(
            "You are an AI coding assistant working on branch '{branch}' in the git worktree at {wt}.\
             This is your working directory — use it for all file operations and git commands.\
             Do not cd to any other directory.\
             Any path preceded by '@' (e.g. @src/main.rs) is a reference to a file path in the git repository.{claude_md}{tool_guidance}"
        ),
        _ => format!(
            "You are an AI assistant helping manage the git repository at {repo_path}.\
             You can inspect code, answer questions, and help coordinate work across branches.\
             Any path preceded by '@' (e.g. @src/main.rs) is a reference to a file path in the git repository.{claude_md}{tool_guidance}"
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
    if std::env::var("CLAUDULHU_SKIP_SHELL_ENV").is_ok() {
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
