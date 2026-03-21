use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{Method, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

// ── Config ────────────────────────────────────────────────────────────────────

fn config_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".claudulhu")
        .join("config.json")
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Config {
    repo:    Option<String>,
    api_key: Option<String>,
    model:   Option<String>,
}

fn read_config() -> Config {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn effective_repo(cfg: &Config) -> String {
    cfg.repo.clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        })
}

fn write_config(cfg: &Config) {
    let path = config_path();
    fs::create_dir_all(path.parent().unwrap()).ok();
    fs::write(path, serde_json::to_string(cfg).unwrap()).ok();
}

fn resolve_api_key() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty())
        .or_else(|| read_config().api_key)
        .or_else(|| read_key_from_shell_files())
}

fn read_key_from_shell_files() -> Option<String> {
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

// ── App State ─────────────────────────────────────────────────────────────────

struct Session {
    messages:         Vec<ApiMessage>,
    system_prompt:    String,
    cwd:              String,
    aborted:          Arc<AtomicBool>,
    pending_question: Arc<tokio::sync::Mutex<Option<oneshot::Sender<String>>>>,
}

struct AppState {
    /// Active sessions keyed by session_id
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
    /// Worker sessions created by spawn_worker, waiting for WS connection (keyed by branch)
    worker_sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
}

// ── API Types ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
struct AnthropicTool {
    name:         String,
    description:  String,
    input_schema: serde_json::Value,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ApiMessage {
    role:    String,
    content: Vec<ContentBlock>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: Vec<serde_json::Value> },
}

// ── Chat Events (server → client) ─────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatEvent {
    Ready              { session_id: String, resumed: bool },
    Text               { text: String },
    ToolUse            { tool: String, input: serde_json::Value },
    ToolResult         { tool_use_id: String, content: serde_json::Value },
    Result             { cost_usd: f64, turns: usize, session_id: String, result: Option<String> },
    Error              { message: String },
    Interrupted,
    Question           { question: String },
    System             { text: String },
    Spawning           { task: String },
    WorkerCreated      { branch: String, worktree_path: String, task: String },
    WorkerError        { message: String },
    WorkerSessionReady { branch: String, worktree_path: String, worker_session_id: String, task: String },
}

// ── Client Messages (client → server, WebSocket) ─────────────────────────────

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Message     { text: String },
    Interrupt,
    SpawnWorker { task: String },
    Answer      { answer: String },
}

// ── Branch ────────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
struct Branch {
    name:     String,
    commit:   String,
    worktree: Option<String>,
}

// ── Task Management ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Task {
    id:          String,
    subject:     String,
    description: String,
    active_form: Option<String>,
    status:      String,
    owner:       Option<String>,
    output:      Option<String>,
    blocks:      Vec<String>,
    blocked_by:  Vec<String>,
    created_at:  u64,
    updated_at:  u64,
}

#[derive(Serialize, Deserialize, Default)]
struct TaskStore {
    next_id: u32,
    tasks:   Vec<Task>,
}

fn tasks_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".claudulhu")
        .join("tasks.json")
}

fn read_task_store() -> TaskStore {
    fs::read_to_string(tasks_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_task_store(store: &TaskStore) {
    let path = tasks_path();
    fs::create_dir_all(path.parent().unwrap()).ok();
    fs::write(path, serde_json::to_string_pretty(store).unwrap()).ok();
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Tool Execution ────────────────────────────────────────────────────────────

fn strip_html(html: &str) -> String {
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

fn resolve_path(p: &str, cwd: &str) -> PathBuf {
    if p.starts_with('/') { PathBuf::from(p) } else { PathBuf::from(cwd).join(p) }
}

const TOOL_OUTPUT_LIMIT: usize = 20_000;

fn truncate_tool_output(s: String) -> String {
    if s.len() <= TOOL_OUTPUT_LIMIT { return s; }
    format!(
        "{}\n[output truncated — {} chars omitted]",
        &s[..TOOL_OUTPUT_LIMIT],
        s.len() - TOOL_OUTPUT_LIMIT,
    )
}

async fn execute_tool(
    name:             &str,
    input:            &serde_json::Value,
    cwd:              &str,
    tx:               &mpsc::Sender<ChatEvent>,
    pending_question: Arc<tokio::sync::Mutex<Option<oneshot::Sender<String>>>>,
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
                    let combined = if stderr.is_empty() { stdout }
                                   else { format!("{stdout}\n[stderr]: {stderr}") };
                    truncate_tool_output(combined)
                }
                Err(e) => format!("error: {e}"),
            }
        }
        "read_file" => {
            let p      = input["path"].as_str().unwrap_or("");
            let full   = resolve_path(p, cwd);
            let offset = input["offset"].as_u64().unwrap_or(0) as usize;
            let limit  = input["limit"].as_u64().map(|v| v as usize);
            match fs::read_to_string(&full) {
                Err(e) => format!("error: {e}"),
                Ok(content) => {
                    let lines: Vec<&str> = content.lines().collect();
                    let total = lines.len();
                    let start = offset.min(total);
                    let end   = limit.map(|l| (start + l).min(total)).unwrap_or(total);
                    let numbered: Vec<String> = lines[start..end]
                        .iter().enumerate()
                        .map(|(i, l)| format!("{:>4}→{}", start + i + 1, l))
                        .collect();
                    if offset > 0 || limit.is_some() {
                        format!("(lines {}-{} of {})\n{}", start+1, end, total, numbered.join("\n"))
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
            match tokio::process::Command::new("grep")
                .args(["-r", "-n", pattern, path])
                .current_dir(cwd).output().await
            {
                Ok(o)  => truncate_tool_output(String::from_utf8_lossy(&o.stdout).to_string()),
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
            let client = reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (compatible; claudulhu/1.0)")
                .build().unwrap();
            match client.get(url).send().await {
                Err(e)   => format!("error: {e}"),
                Ok(resp) => {
                    let status = resp.status();
                    match resp.text().await {
                        Err(e)   => format!("error reading response: {e}"),
                        Ok(body) => {
                            let text = strip_html(&body);
                            let truncated = if text.len() > 50_000 {
                                format!("{}\n[truncated at 50000 chars]", &text[..50_000])
                            } else { text };
                            if status.is_success() { truncated }
                            else { format!("HTTP {status}\n{truncated}") }
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
            let client = reqwest::Client::new();
            match client
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
        _ => format!("unknown tool: {name}"),
    }
}

// ── Tool Definitions ──────────────────────────────────────────────────────────

fn tool_definitions() -> Vec<AnthropicTool> {
    vec![
        AnthropicTool { name: "bash".into(), description: "Run a shell command in the repository directory. Returns stdout/stderr.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] }) },
        AnthropicTool { name: "read_file".into(), description: "Read a file, optionally a line range. Use offset+limit to read only the section you need. Lines are returned with line numbers.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" }, "offset": { "type": "integer" }, "limit": { "type": "integer" } }, "required": ["path"] }) },
        AnthropicTool { name: "edit_file".into(), description: "Replace an exact string in a file. PREFER this over write_file for modifying existing files. old_str must match exactly once.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" }, "old_str": { "type": "string" }, "new_str": { "type": "string" } }, "required": ["path", "old_str", "new_str"] }) },
        AnthropicTool { name: "write_file".into(), description: "Write a file. Use for creating new files only; prefer edit_file for existing files.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" }, "content": { "type": "string" } }, "required": ["path", "content"] }) },
        AnthropicTool { name: "glob".into(), description: "Find files matching a glob pattern (e.g. src/**/*.rs).".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "pattern": { "type": "string" } }, "required": ["pattern"] }) },
        AnthropicTool { name: "grep".into(), description: "Search file contents for a regex pattern. Returns matching lines with line numbers.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "pattern": { "type": "string" }, "path": { "type": "string" } }, "required": ["pattern"] }) },
        AnthropicTool { name: "ask_user".into(), description: "Pause and ask the user a clarifying question. Returns the user's answer.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "question": { "type": "string" } }, "required": ["question"] }) },
        AnthropicTool { name: "task_create".into(), description: "Create a task with status 'pending'. Returns the task ID.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "subject": { "type": "string" }, "description": { "type": "string" }, "activeForm": { "type": "string" } }, "required": ["subject", "description"] }) },
        AnthropicTool { name: "task_list".into(), description: "List all non-deleted tasks.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": {} }) },
        AnthropicTool { name: "task_get".into(), description: "Get full details of a task by ID.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "taskId": { "type": "string" } }, "required": ["taskId"] }) },
        AnthropicTool { name: "task_update".into(), description: "Update a task's status, subject, description, owner, or dependencies.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "taskId": { "type": "string" }, "status": { "type": "string", "enum": ["pending","in_progress","completed","deleted"] }, "subject": { "type": "string" }, "description": { "type": "string" }, "activeForm": { "type": "string" }, "owner": { "type": "string" }, "addBlocks": { "type": "array", "items": { "type": "string" } }, "addBlockedBy": { "type": "array", "items": { "type": "string" } } }, "required": ["taskId"] }) },
        AnthropicTool { name: "task_stop".into(), description: "Cancel (delete) a task by ID.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "task_id": { "type": "string" } }, "required": ["task_id"] }) },
        AnthropicTool { name: "task_output".into(), description: "Get the output field of a task.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "task_id": { "type": "string" } }, "required": ["task_id"] }) },
        AnthropicTool { name: "web_fetch".into(), description: "Fetch a URL and return its text content (HTML stripped). Truncated at 50 000 chars.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"] }) },
        AnthropicTool { name: "web_search".into(), description: "Search the web via Brave Search. Requires BRAVE_API_KEY env var.".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "query": { "type": "string" } }, "required": ["query"] }) },
    ]
}

// ── Anthropic Streaming ───────────────────────────────────────────────────────

struct StreamUsage { input_tokens: u64, output_tokens: u64 }

enum PartialBlock {
    Text    { text: String },
    ToolUse { id: String, name: String, partial_json: String },
}

fn compact_history(messages: &[ApiMessage], keep_full: usize) -> Vec<ApiMessage> {
    const STUB_LIMIT: usize = 400;
    let tool_result_indices: Vec<usize> = messages.iter().enumerate()
        .filter(|(_, m)| m.role == "user" && m.content.iter().all(|b| matches!(b, ContentBlock::ToolResult { .. })))
        .map(|(i, _)| i).collect();

    let cutoff = tool_result_indices.len().saturating_sub(keep_full);
    let old_indices: std::collections::HashSet<usize> =
        tool_result_indices[..cutoff].iter().copied().collect();

    messages.iter().enumerate().map(|(i, m)| {
        if !old_indices.contains(&i) { return m.clone(); }
        ApiMessage {
            role: m.role.clone(),
            content: m.content.iter().map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, content } => {
                    let text = content.first().and_then(|v| v["text"].as_str()).unwrap_or("");
                    let stub = if text.len() > STUB_LIMIT {
                        format!("{}…[truncated]", &text[..STUB_LIMIT])
                    } else { text.to_string() };
                    ContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: vec![serde_json::json!({"type":"text","text":stub})],
                    }
                }
                other => other.clone(),
            }).collect(),
        }
    }).collect()
}

async fn stream_turn(
    messages:  &[ApiMessage],
    system:    &str,
    model:     &str,
    api_key:   &str,
    aborted:   &AtomicBool,
    tx:        &mpsc::Sender<ChatEvent>,
) -> Result<(Vec<ContentBlock>, String, StreamUsage), String> {
    let client = reqwest::Client::new();

    let mut tools: Vec<serde_json::Value> = tool_definitions()
        .into_iter().map(|t| serde_json::to_value(t).unwrap()).collect();
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = serde_json::json!({"type": "ephemeral"});
    }

    let compacted = compact_history(messages, 6);

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 16000,
        "system": [{"type":"text","text":system,"cache_control":{"type":"ephemeral"}}],
        "tools": tools,
        "messages": compacted,
        "stream": true,
    });

    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "prompt-caching-2024-07-31")
        .header("content-type", "application/json")
        .json(&body).send().await.map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        let status = response.status();
        let text   = response.text().await.unwrap_or_default();
        return Err(format!("API error {status}: {text}"));
    }

    let mut stream = response.bytes_stream();
    let mut buf    = String::new();
    let mut partial: HashMap<usize, PartialBlock> = HashMap::new();
    let mut completed: Vec<(usize, ContentBlock)> = Vec::new();
    let mut input_tokens:  u64 = 0;
    let mut output_tokens: u64 = 0;
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
                    if let Some(u) = ev["message"]["usage"]["input_tokens"].as_u64() {
                        input_tokens = u;
                    }
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
    Ok((blocks, stop_reason, StreamUsage { input_tokens, output_tokens }))
}

// ── Cost ──────────────────────────────────────────────────────────────────────

fn cost_usd(model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
    let (input_rate, output_rate) = if model.contains("opus") { (15.0, 75.0) } else { (3.0, 15.0) };
    (input_tokens as f64 * input_rate + output_tokens as f64 * output_rate) / 1_000_000.0
}

// ── Agentic Loop ──────────────────────────────────────────────────────────────

async fn run_agentic_loop(
    session:    Arc<Mutex<Session>>,
    session_id: String,
    api_key:    String,
    model:      String,
    tx:         mpsc::Sender<ChatEvent>,
) {
    let mut turns         = 0usize;
    let mut total_input   = 0u64;
    let mut total_output  = 0u64;

    loop {
        let (messages, system, cwd, aborted, pending_question) = {
            let s = session.lock().unwrap();
            (s.messages.clone(), s.system_prompt.clone(), s.cwd.clone(),
             s.aborted.clone(), s.pending_question.clone())
        };

        if aborted.load(Ordering::Relaxed) {
            tx.send(ChatEvent::Interrupted).await.ok();
            return;
        }

        match stream_turn(&messages, &system, &model, &api_key, &aborted, &tx).await {
            Err(e) if e == "__interrupted__" => {
                tx.send(ChatEvent::Interrupted).await.ok();
                return;
            }
            Err(e) => {
                tx.send(ChatEvent::Error { message: e }).await.ok();
                return;
            }
            Ok((blocks, stop_reason, usage)) => {
                turns        += 1;
                total_input  += usage.input_tokens;
                total_output += usage.output_tokens;

                {
                    let mut s = session.lock().unwrap();
                    s.messages.push(ApiMessage { role: "assistant".to_string(), content: blocks.clone() });
                }

                if stop_reason != "tool_use" {
                    let cost = cost_usd(&model, total_input, total_output);
                    tx.send(ChatEvent::Result {
                        cost_usd: cost, turns, session_id: session_id.clone(), result: None,
                    }).await.ok();
                    return;
                }

                let mut tool_results: Vec<ContentBlock> = Vec::new();
                for block in &blocks {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let result = execute_tool(name, input, &cwd, &tx, pending_question.clone()).await;
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

                { session.lock().unwrap().messages.push(ApiMessage { role: "user".to_string(), content: tool_results }); }
            }
        }
    }
}

// ── System Prompt ─────────────────────────────────────────────────────────────

fn build_system_prompt(repo_path: &str, branch: Option<&str>, worktree_path: Option<&str>) -> String {
    let tool_guidance = "\n\nTool use guidelines (IMPORTANT — follow to minimise token cost):\
        \n- To modify an existing file use edit_file (str_replace). Never read the whole file just to rewrite it.\
        \n- Use read_file with offset+limit to read only the section you need.\
        \n- Use grep to locate the exact lines before reading or editing.\
        \n- Use write_file only for creating new files.\
        \n- Be concise and precise.";

    let claude_md = std::fs::read_to_string(format!("{}/CLAUDE.md", repo_path))
        .map(|s| format!("\n\n# Project instructions (CLAUDE.md)\n{}", s))
        .unwrap_or_default();

    match (branch, worktree_path) {
        (Some(branch), Some(wt)) => format!(
            "You are an AI coding assistant working on branch '{branch}' of the git repository at {repo_path}.\
             Your working directory is the worktree at {wt}.{claude_md}{tool_guidance}"
        ),
        _ => format!(
            "You are an AI assistant helping manage the git repository at {repo_path}.\
             You can inspect code, answer questions, and help coordinate work across branches.{claude_md}{tool_guidance}"
        ),
    }
}

// ── Git ───────────────────────────────────────────────────────────────────────

fn get_branches_for_repo(repo: &str) -> Result<Vec<Branch>, String> {
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

fn slug(text: &str) -> String {
    let s: String = text.to_lowercase().chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let parts: Vec<&str> = s.split('-').filter(|p| !p.is_empty()).collect();
    parts.join("-").chars().take(40).collect()
}

async fn generate_branch_name(task: &str, api_key: &str) -> String {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 32,
        "messages": [{ "role": "user", "content": format!(
            "Generate a short git branch name (2-4 words, lowercase, hyphenated, no punctuation) \
             for this task: {task}\n\nReply with only the branch name, nothing else."
        )}]
    });
    let name: String = async {
        let resp = client
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

fn create_worktree(repo_path: &str, branch: &str) -> Result<String, String> {
    let repo_name = PathBuf::from(repo_path).file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let worktree_path = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".claudulhu").join("worktrees").join(&repo_name).join(branch);
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

// ── Spawn Worker ──────────────────────────────────────────────────────────────

async fn spawn_worker(
    app_state:  &Arc<AppState>,
    _session_id: &str,
    task:       &str,
    repo:       &str,
    tx:         mpsc::Sender<ChatEvent>,
) {
    tx.send(ChatEvent::Spawning { task: task.to_string() }).await.ok();

    let api_key = resolve_api_key().unwrap_or_default();
    let branch  = generate_branch_name(task, &api_key).await;
    let branch  = if branch.is_empty() { Uuid::new_v4().to_string()[..8].to_string() } else { branch };

    let worktree_path = match create_worktree(repo, &branch) {
        Ok(p)  => p,
        Err(e) => { tx.send(ChatEvent::WorkerError { message: e }).await.ok(); return; }
    };

    tx.send(ChatEvent::WorkerCreated { branch: branch.clone(), worktree_path: worktree_path.clone(), task: task.to_string() }).await.ok();

    let worker_session_id = Uuid::new_v4().to_string();
    let system_prompt     = build_system_prompt(repo, Some(&branch), Some(&worktree_path));
    let worker_session    = Arc::new(Mutex::new(Session {
        messages:         Vec::new(),
        system_prompt,
        cwd:              worktree_path.clone(),
        aborted:          Arc::new(AtomicBool::new(false)),
        pending_question: Arc::new(tokio::sync::Mutex::new(None)),
    }));

    app_state.worker_sessions.lock().unwrap().insert(branch.clone(), worker_session.clone());

    tx.send(ChatEvent::WorkerSessionReady {
        branch:            branch.clone(),
        worktree_path:     worktree_path.clone(),
        worker_session_id: worker_session_id.clone(),
        task:              task.to_string(),
    }).await.ok();

    // Also register in sessions map so the worker WS can look it up by ID if needed
    app_state.sessions.lock().unwrap().insert(worker_session_id, worker_session);
}

// ── WebSocket Session Handler ─────────────────────────────────────────────────
//
// Shared logic for both the main /chat and /workers/:branch connections.

async fn run_session(socket: WebSocket, session: Arc<Mutex<Session>>, session_id: String, app_state: Arc<AppState>, repo: String) {
    let (mut ws_sink, mut ws_stream) = socket.split();

    // Event channel: agent loop → WS sender
    let (tx, mut rx) = mpsc::channel::<ChatEvent>(256);

    // Spawn task that forwards events from channel to the WebSocket
    let send_task = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let json = match serde_json::to_string(&event) {
                Ok(s)  => s,
                Err(_) => continue,
            };
            if ws_sink.send(Message::Text(json)).await.is_err() { break; }
        }
    });

    // Send Ready
    tx.send(ChatEvent::Ready { session_id: session_id.clone(), resumed: false }).await.ok();

    // Handle incoming client messages
    while let Some(Ok(msg)) = ws_stream.next().await {
        let text = match msg {
            Message::Text(t)   => t,
            Message::Close(_)  => break,
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Binary(_) => continue,
        };

        let client_msg: ClientMessage = match serde_json::from_str(&text) {
            Ok(m)  => m,
            Err(_) => continue,
        };

        match client_msg {
            ClientMessage::Message { text } => {
                let cfg     = read_config();
                let api_key = match resolve_api_key() {
                    Some(k) => k,
                    None    => { tx.send(ChatEvent::Error { message: "no API key configured".to_string() }).await.ok(); continue; }
                };
                let model = cfg.model.unwrap_or_else(|| "claude-sonnet-4-6".to_string());

                {
                    let mut s = session.lock().unwrap();
                    s.aborted.store(false, Ordering::Relaxed);
                    s.messages.push(ApiMessage {
                        role:    "user".to_string(),
                        content: vec![ContentBlock::Text { text }],
                    });
                }

                let session_clone = session.clone();
                let sid_clone     = session_id.clone();
                let tx_clone      = tx.clone();
                tokio::spawn(async move {
                    run_agentic_loop(session_clone, sid_clone, api_key, model, tx_clone).await;
                });
            }

            ClientMessage::Interrupt => {
                session.lock().unwrap().aborted.store(true, Ordering::Relaxed);
            }

            ClientMessage::SpawnWorker { task } => {
                let state_clone = app_state.clone();
                let sid_clone   = session_id.clone();
                let tx_clone    = tx.clone();
                let repo_clone  = repo.clone();
                tokio::spawn(async move {
                    spawn_worker(&state_clone, &sid_clone, &task, &repo_clone, tx_clone).await;
                });
            }

            ClientMessage::Answer { answer } => {
                let pending_question = session.lock().unwrap().pending_question.clone();
                let mut slot = pending_question.lock().await;
                if let Some(sender) = slot.take() { sender.send(answer).ok(); }
            }
        }
    }

    // WS closed — abort any running agent
    session.lock().unwrap().aborted.store(true, Ordering::Relaxed);
    send_task.abort();
}

// ── HTTP Handlers ─────────────────────────────────────────────────────────────

async fn get_branches_handler(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let cfg = read_config();
    let repo = effective_repo(&cfg);
    match get_branches_for_repo(&repo) {
        Ok(branches) => Json(branches).into_response(),
        Err(e)       => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_config_handler() -> Json<Config> {
    Json(read_config())
}

// Accept a partial config — only provided fields are updated
async fn update_config_handler(Json(patch): Json<Config>) -> StatusCode {
    let mut cfg = read_config();
    if patch.repo.is_some()    { cfg.repo    = patch.repo; }
    if patch.api_key.is_some() { cfg.api_key = patch.api_key; }
    if patch.model.is_some()   { cfg.model   = patch.model; }
    write_config(&cfg);
    StatusCode::OK
}

// ── WebSocket Route Handlers ──────────────────────────────────────────────────

async fn chat_ws_handler(
    ws:              WebSocketUpgrade,
    State(state):    State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let session_id = Uuid::new_v4().to_string();
        let cfg        = read_config();
        let repo       = effective_repo(&cfg);
        let system     = build_system_prompt(&repo, None, None);

        let session = Arc::new(Mutex::new(Session {
            messages:         Vec::new(),
            system_prompt:    system,
            cwd:              repo.clone(),
            aborted:          Arc::new(AtomicBool::new(false)),
            pending_question: Arc::new(tokio::sync::Mutex::new(None)),
        }));
        state.sessions.lock().unwrap().insert(session_id.clone(), session.clone());

        run_session(socket, session, session_id.clone(), state.clone(), repo).await;

        state.sessions.lock().unwrap().remove(&session_id);
    })
}

async fn worker_ws_handler(
    ws:           WebSocketUpgrade,
    Path(branch): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        // Look up a pre-created worker session (spawned by spawn_worker on the main WS)
        let existing = state.worker_sessions.lock().unwrap().remove(&branch);

        let (session, repo) = if let Some(sess) = existing {
            let cwd = sess.lock().unwrap().cwd.clone();
            (sess, cwd)
        } else {
            // Fallback: create a fresh worker session if none was pre-created
            let cfg  = read_config();
            let repo = effective_repo(&cfg);
            let system = build_system_prompt(&repo, Some(&branch), None);
            let sess = Arc::new(Mutex::new(Session {
                messages:         Vec::new(),
                system_prompt:    system,
                cwd:              repo.clone(),
                aborted:          Arc::new(AtomicBool::new(false)),
                pending_question: Arc::new(tokio::sync::Mutex::new(None)),
            }));
            (sess, repo)
        };

        let session_id = Uuid::new_v4().to_string();
        state.sessions.lock().unwrap().insert(session_id.clone(), session.clone());

        run_session(socket, session, session_id.clone(), state.clone(), repo).await;

        state.sessions.lock().unwrap().remove(&session_id);
    })
}

// ── Shell Environment Bootstrap ───────────────────────────────────────────────

fn init_shell_env() {
    let output = std::process::Command::new("zsh")
        .args(["-l", "-c", "source ~/.zshrc 2>/dev/null; env -0"])
        .output();
    let Ok(output) = output else { return };
    let Ok(env_str) = std::str::from_utf8(&output.stdout) else { return };
    for entry in env_str.split('\0') {
        if let Some((key, val)) = entry.split_once('=') {
            std::env::set_var(key, val);
        }
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    init_shell_env();

    let state = Arc::new(AppState {
        sessions:        Mutex::new(HashMap::new()),
        worker_sessions: Mutex::new(HashMap::new()),
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::PUT, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let app = Router::new()
        .route("/branches",        get(get_branches_handler))
        .route("/config",          get(get_config_handler).put(update_config_handler))
        .route("/chat",            get(chat_ws_handler))
        .route("/workers/:branch", get(worker_ws_handler))
        .with_state(state)
        .layer(cors);

    let addr = "0.0.0.0:8000";
    let listener = tokio::net::TcpListener::bind(addr).await
        .expect("failed to bind to port 8000");
    println!("claudulhu server listening on {addr}");
    println!("  WebSocket: ws://{addr}/chat");
    println!("  WebSocket: ws://{addr}/workers/:branch");
    println!("  HTTP GET:  http://{addr}/branches");
    println!("  HTTP GET:  http://{addr}/config");
    println!("  HTTP PUT:  http://{addr}/config");

    axum::serve(listener, app).await.unwrap();
}
