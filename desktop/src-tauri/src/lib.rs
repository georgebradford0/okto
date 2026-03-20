use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::oneshot;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

// ── Config ────────────────────────────────────────────────────────────────────

fn config_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".claudulhu")
        .join("config.json")
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Config {
    repo: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
}

fn read_config() -> Config {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_config(cfg: &Config) {
    let path = config_path();
    fs::create_dir_all(path.parent().unwrap()).ok();
    fs::write(path, serde_json::to_string(cfg).unwrap()).ok();
}

// ── App State ─────────────────────────────────────────────────────────────────

struct Session {
    messages: Vec<ApiMessage>,
    system_prompt: String,
    cwd: String,
    aborted: Arc<AtomicBool>,
    pending_question: Arc<tokio::sync::Mutex<Option<oneshot::Sender<String>>>>,
}

struct AppState {
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
}

// ── API Types ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ApiMessage {
    role: String,
    content: Vec<ContentBlock>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<serde_json::Value>,
    },
}

// ── Chat Events (emitted to frontend) ────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatEvent {
    Ready {
        session_id: String,
        resumed: bool,
    },
    Text {
        text: String,
    },
    ToolUse {
        tool: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: serde_json::Value,
    },
    Result {
        cost_usd: f64,
        turns: usize,
        session_id: String,
        result: Option<String>,
    },
    Error {
        message: String,
    },
    Interrupted,
    Question {
        question: String,
    },
    Spawning {
        task: String,
    },
    WorkerCreated {
        branch: String,
        worktree_path: String,
        task: String,
    },
    WorkerError {
        message: String,
    },
    WorkerSessionReady {
        branch: String,
        worktree_path: String,
        worker_session_id: String,
        task: String,
    },
}

fn emit(app: &AppHandle, session_id: &str, event: ChatEvent) {
    let channel = format!("claude-event-{session_id}");
    app.emit(&channel, event).ok();
}

// ── Git ───────────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
struct Branch {
    name: String,
    commit: String,
    worktree: Option<String>,
}

#[tauri::command]
fn get_branches(repo: String) -> Result<Vec<Branch>, String> {
    let repo_obj = git2::Repository::open(&repo).map_err(|e| e.to_string())?;

    // Map branch name → worktree path by inspecting each worktree
    let mut worktree_map: HashMap<String, String> = HashMap::new();
    if let Ok(names) = repo_obj.worktrees() {
        for wt_name in names.iter().flatten() {
            if let Ok(wt) = repo_obj.find_worktree(wt_name) {
                let path = wt.path();
                if let Ok(wt_repo) = git2::Repository::open(path) {
                    if let Ok(head) = wt_repo.head() {
                        if let Some(short) = head.shorthand() {
                            worktree_map.insert(
                                short.to_string(),
                                path.to_string_lossy().to_string(),
                            );
                        }
                    }
                }
            }
        }
    }

    let mut branches = Vec::new();
    let iter = repo_obj
        .branches(Some(git2::BranchType::Local))
        .map_err(|e| e.to_string())?;

    for item in iter {
        let (b, _) = item.map_err(|e| e.to_string())?;
        let name = b.name().ok().flatten().unwrap_or("").to_string();
        let commit = b
            .get()
            .peel_to_commit()
            .map(|c| c.id().to_string()[..7].to_string())
            .unwrap_or_default();
        let worktree = worktree_map.get(&name).cloned();
        branches.push(Branch { name, commit, worktree });
    }

    Ok(branches)
}

fn slug(text: &str) -> String {
    let s: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let parts: Vec<&str> = s.split('-').filter(|p| !p.is_empty()).collect();
    let joined = parts.join("-");
    joined.chars().take(40).collect()
}

/// Ask Claude for a short, descriptive branch name for the given task.
/// Falls back to slug() if the API call fails.
async fn generate_branch_name(task: &str, api_key: &str) -> String {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 32,
        "messages": [{
            "role": "user",
            "content": format!(
                "Generate a short git branch name (2-4 words, lowercase, hyphenated, no punctuation) \
                 for this task: {task}\n\nReply with only the branch name, nothing else."
            )
        }]
    });

    let result = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await;

    let name: String = async {
        let resp = result.ok()?;
        if !resp.status().is_success() { return None; }
        let v: serde_json::Value = resp.json().await.ok()?;
        let text = v["content"][0]["text"].as_str()?.trim().to_string();
        Some(text)
    }.await.unwrap_or_default();

    let cleaned = slug(&name);
    if cleaned.is_empty() { slug(task) } else { cleaned }
}

fn create_worktree(repo_path: &str, branch: &str) -> Result<String, String> {
    let repo_name = PathBuf::from(repo_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());

    let worktree_path = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".claudulhu")
        .join("worktrees")
        .join(&repo_name)
        .join(branch);

    if let Some(parent) = worktree_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let out = std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            branch,
            &worktree_path.to_string_lossy(),
            "HEAD",
        ])
        .current_dir(repo_path)
        .output()
        .map_err(|e| e.to_string())?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }

    Ok(worktree_path.to_string_lossy().to_string())
}

// ── Task Management ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Task {
    id: String,
    subject: String,
    description: String,
    active_form: Option<String>,
    status: String, // "pending" | "in_progress" | "completed" | "deleted"
    owner: Option<String>,
    output: Option<String>,
    blocks: Vec<String>,
    blocked_by: Vec<String>,
    created_at: u64,
    updated_at: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct TaskStore {
    next_id: u32,
    tasks: Vec<Task>,
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
    let mut in_tag = false;
    let mut in_script = false;
    let mut tag_buf = String::new();

    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' => {
                let tag = tag_buf.trim().to_lowercase();
                if tag.starts_with("script") || tag.starts_with("style") {
                    in_script = true;
                } else if tag.starts_with("/script") || tag.starts_with("/style") {
                    in_script = false;
                }
                in_tag = false;
            }
            _ if in_tag => {
                tag_buf.push(c);
            }
            _ if !in_script => {
                out.push(c);
            }
            _ => {}
        }
    }
    // Normalise whitespace
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn resolve_path(p: &str, cwd: &str) -> PathBuf {
    if p.starts_with('/') { PathBuf::from(p) } else { PathBuf::from(cwd).join(p) }
}

async fn execute_tool(
    name: &str,
    input: &serde_json::Value,
    cwd: &str,
    app: &AppHandle,
    session_id: &str,
    pending_question: Arc<tokio::sync::Mutex<Option<oneshot::Sender<String>>>>,
) -> String {
    match name {
        "bash" => {
            let cmd = input["command"].as_str().unwrap_or("");
            let wrapped = format!("source ~/.zshrc 2>/dev/null; {}", cmd);
            match tokio::process::Command::new("zsh")
                .args(["-l", "-c", &wrapped])
                .current_dir(cwd)
                .output()
                .await
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
            let p = input["path"].as_str().unwrap_or("");
            let full = resolve_path(p, cwd);
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
                        .iter()
                        .enumerate()
                        .map(|(i, l)| format!("{:>4}→{}", start + i + 1, l))
                        .collect();
                    if offset > 0 || limit.is_some() {
                        format!("(lines {}-{} of {})\n{}", start + 1, end, total, numbered.join("\n"))
                    } else {
                        numbered.join("\n")
                    }
                }
            }
        }
        "edit_file" => {
            // Targeted str_replace — only sends the changed lines, not the whole file
            let p        = input["path"].as_str().unwrap_or("");
            let old_str  = input["old_str"].as_str().unwrap_or("");
            let new_str  = input["new_str"].as_str().unwrap_or("");
            let full = resolve_path(p, cwd);
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
            // Use for new files only — prefer edit_file for existing files
            let p       = input["path"].as_str().unwrap_or("");
            let content = input["content"].as_str().unwrap_or("");
            let full = resolve_path(p, cwd);
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
                    .collect::<Vec<_>>()
                    .join("\n"),
                Err(e) => format!("error: {e}"),
            }
        }
        "grep" => {
            let pattern = input["pattern"].as_str().unwrap_or("");
            let path    = input["path"].as_str().unwrap_or(".");
            match tokio::process::Command::new("grep")
                .args(["-r", "-n", pattern, path])
                .current_dir(cwd)
                .output()
                .await
            {
                Ok(o)  => String::from_utf8_lossy(&o.stdout).to_string(),
                Err(e) => format!("error: {e}"),
            }
        }
        "ask_user" => {
            let question = input["question"].as_str().unwrap_or("").to_string();
            if question.is_empty() {
                return "error: question is required".to_string();
            }
            let (tx, rx) = oneshot::channel::<String>();
            {
                let mut slot = pending_question.lock().await;
                *slot = Some(tx);
            }
            emit(app, session_id, ChatEvent::Question { question });
            match rx.await {
                Ok(answer) => answer,
                Err(_) => "error: question was cancelled".to_string(),
            }
        }
        "task_create" => {
            let subject = input["subject"].as_str().unwrap_or("").to_string();
            if subject.is_empty() {
                return "error: subject is required".to_string();
            }
            let description = input["description"].as_str().unwrap_or("").to_string();
            let active_form = input["activeForm"].as_str().map(|s| s.to_string());
            let now = now_secs();
            let mut store = read_task_store();
            store.next_id += 1;
            let id = store.next_id.to_string();
            store.tasks.push(Task {
                id: id.clone(),
                subject,
                description,
                active_form,
                status: "pending".to_string(),
                owner: None,
                output: None,
                blocks: vec![],
                blocked_by: vec![],
                created_at: now,
                updated_at: now,
            });
            write_task_store(&store);
            format!("created task {id}")
        }
        "task_list" => {
            let store = read_task_store();
            let visible: Vec<&Task> = store.tasks.iter()
                .filter(|t| t.status != "deleted")
                .collect();
            if visible.is_empty() {
                return "no tasks".to_string();
            }
            visible.iter().map(|t| {
                let blocked = if t.blocked_by.is_empty() {
                    String::new()
                } else {
                    format!(" [blocked by: {}]", t.blocked_by.join(", "))
                };
                let owner = t.owner.as_deref().map(|o| format!(" owner={o}")).unwrap_or_default();
                format!("[{}] {} — {}{}{}", t.id, t.status, t.subject, owner, blocked)
            }).collect::<Vec<_>>().join("\n")
        }
        "task_get" => {
            let id = input["taskId"].as_str().unwrap_or("");
            let store = read_task_store();
            match store.tasks.iter().find(|t| t.id == id) {
                None => format!("error: task {id} not found"),
                Some(t) => serde_json::to_string_pretty(t).unwrap_or_default(),
            }
        }
        "task_update" => {
            let id = input["taskId"].as_str().unwrap_or("");
            let mut store = read_task_store();
            match store.tasks.iter_mut().find(|t| t.id == id) {
                None => format!("error: task {id} not found"),
                Some(t) => {
                    if let Some(s) = input["status"].as_str() {
                        t.status = s.to_string();
                    }
                    if let Some(s) = input["subject"].as_str() {
                        t.subject = s.to_string();
                    }
                    if let Some(s) = input["description"].as_str() {
                        t.description = s.to_string();
                    }
                    if let Some(s) = input["activeForm"].as_str() {
                        t.active_form = Some(s.to_string());
                    }
                    if let Some(s) = input["owner"].as_str() {
                        t.owner = Some(s.to_string());
                    }
                    if let Some(arr) = input["addBlocks"].as_array() {
                        for v in arr {
                            if let Some(s) = v.as_str() {
                                if !t.blocks.contains(&s.to_string()) {
                                    t.blocks.push(s.to_string());
                                }
                            }
                        }
                    }
                    if let Some(arr) = input["addBlockedBy"].as_array() {
                        for v in arr {
                            if let Some(s) = v.as_str() {
                                if !t.blocked_by.contains(&s.to_string()) {
                                    t.blocked_by.push(s.to_string());
                                }
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
            let id = input["task_id"].as_str()
                .or_else(|| input["taskId"].as_str())
                .unwrap_or("");
            let mut store = read_task_store();
            match store.tasks.iter_mut().find(|t| t.id == id) {
                None => format!("error: task {id} not found"),
                Some(t) => {
                    t.status = "deleted".to_string();
                    t.updated_at = now_secs();
                    write_task_store(&store);
                    "ok".to_string()
                }
            }
        }
        "task_output" => {
            let id = input["task_id"].as_str().unwrap_or("");
            let store = read_task_store();
            match store.tasks.iter().find(|t| t.id == id) {
                None => format!("error: task {id} not found"),
                Some(t) => t.output.clone().unwrap_or_else(|| "(no output)".to_string()),
            }
        }
        "web_fetch" => {
            let url = input["url"].as_str().unwrap_or("");
            if url.is_empty() {
                return "error: url is required".to_string();
            }
            let client = reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (compatible; claudulhu/1.0)")
                .build()
                .unwrap();
            match client.get(url).send().await {
                Err(e) => format!("error: {e}"),
                Ok(resp) => {
                    let status = resp.status();
                    match resp.text().await {
                        Err(e) => format!("error reading response: {e}"),
                        Ok(body) => {
                            let text = strip_html(&body);
                            let truncated = if text.len() > 50_000 {
                                format!("{}\n[truncated at 50000 chars]", &text[..50_000])
                            } else {
                                text
                            };
                            if status.is_success() {
                                truncated
                            } else {
                                format!("HTTP {status}\n{truncated}")
                            }
                        }
                    }
                }
            }
        }
        "web_search" => {
            let query = input["query"].as_str().unwrap_or("");
            if query.is_empty() {
                return "error: query is required".to_string();
            }
            let api_key = match std::env::var("BRAVE_API_KEY").ok().filter(|s| !s.is_empty()) {
                Some(k) => k,
                None => return "error: BRAVE_API_KEY environment variable not set".to_string(),
            };
            let client = reqwest::Client::new();
            match client
                .get("https://api.search.brave.com/res/v1/web/search")
                .query(&[("q", query), ("count", "10")])
                .header("Accept", "application/json")
                .header("X-Subscription-Token", api_key)
                .send()
                .await
            {
                Err(e) => format!("error: {e}"),
                Ok(resp) => match resp.json::<serde_json::Value>().await {
                    Err(e) => format!("error parsing response: {e}"),
                    Ok(v) => match v["web"]["results"].as_array() {
                        None => "no results".to_string(),
                        Some(items) => items
                            .iter()
                            .map(|r| {
                                let title = r["title"].as_str().unwrap_or("");
                                let url   = r["url"].as_str().unwrap_or("");
                                let desc  = r["description"].as_str().unwrap_or("");
                                format!("**{title}**\n{url}\n{desc}")
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n"),
                    },
                },
            }
        }
        _ => format!("unknown tool: {name}"),
    }
}

fn tool_definitions() -> Vec<AnthropicTool> {
    vec![
        AnthropicTool {
            name: "bash".to_string(),
            description: "Run a shell command in the repository directory. Returns stdout/stderr.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" }
                },
                "required": ["command"]
            }),
        },
        AnthropicTool {
            name: "read_file".to_string(),
            description: "Read a file, optionally a line range. Use offset+limit to read only the section you need — avoids loading large files. Lines are returned with line numbers.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path":   { "type": "string", "description": "File path (relative to repo or absolute)" },
                    "offset": { "type": "integer", "description": "0-based line to start from (default 0)" },
                    "limit":  { "type": "integer", "description": "Max lines to return (omit for whole file)" }
                },
                "required": ["path"]
            }),
        },
        AnthropicTool {
            name: "edit_file".to_string(),
            description: "Replace an exact string in a file. PREFER this over write_file for modifying existing files — only the changed text is needed, not the whole file. old_str must match exactly once.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path":    { "type": "string", "description": "File path" },
                    "old_str": { "type": "string", "description": "Exact string to replace (must be unique in the file)" },
                    "new_str": { "type": "string", "description": "Replacement string" }
                },
                "required": ["path", "old_str", "new_str"]
            }),
        },
        AnthropicTool {
            name: "write_file".to_string(),
            description: "Write a file. Use for creating new files only. For modifying existing files use edit_file instead — it is far cheaper.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path":    { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        },
        AnthropicTool {
            name: "glob".to_string(),
            description: "Find files matching a glob pattern (e.g. src/**/*.rs).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" }
                },
                "required": ["pattern"]
            }),
        },
        AnthropicTool {
            name: "grep".to_string(),
            description: "Search file contents for a regex pattern. Returns matching lines with line numbers.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern" },
                    "path":    { "type": "string", "description": "Directory or file to search (default: .)" }
                },
                "required": ["pattern"]
            }),
        },
        AnthropicTool {
            name: "ask_user".to_string(),
            description: "Pause and ask the user a clarifying question. Use when a task is ambiguous and proceeding incorrectly would waste significant effort. Returns the user's answer as a string.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question to ask the user" }
                },
                "required": ["question"]
            }),
        },
        AnthropicTool {
            name: "task_create".to_string(),
            description: "Create a task with status 'pending'. Use for complex multi-step work. Returns the task ID.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "subject":     { "type": "string", "description": "Brief title in imperative form (e.g. 'Fix auth bug')" },
                    "description": { "type": "string", "description": "Detailed requirements and acceptance criteria" },
                    "activeForm":  { "type": "string", "description": "Present-continuous label shown while in_progress (e.g. 'Fixing auth bug')" }
                },
                "required": ["subject", "description"]
            }),
        },
        AnthropicTool {
            name: "task_list".to_string(),
            description: "List all non-deleted tasks showing id, status, subject, owner, and blockedBy.".to_string(),
            input_schema: serde_json::json!({ "type": "object", "properties": {} }),
        },
        AnthropicTool {
            name: "task_get".to_string(),
            description: "Get full details of a task by ID, including description, blocks, and blockedBy.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "taskId": { "type": "string", "description": "Task ID" }
                },
                "required": ["taskId"]
            }),
        },
        AnthropicTool {
            name: "task_update".to_string(),
            description: "Update a task's status, subject, description, owner, or dependencies. Status values: pending → in_progress → completed | deleted.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "taskId":       { "type": "string" },
                    "status":       { "type": "string", "enum": ["pending", "in_progress", "completed", "deleted"] },
                    "subject":      { "type": "string" },
                    "description":  { "type": "string" },
                    "activeForm":   { "type": "string" },
                    "owner":        { "type": "string" },
                    "addBlocks":    { "type": "array", "items": { "type": "string" }, "description": "Task IDs this task blocks" },
                    "addBlockedBy": { "type": "array", "items": { "type": "string" }, "description": "Task IDs that must complete first" }
                },
                "required": ["taskId"]
            }),
        },
        AnthropicTool {
            name: "task_stop".to_string(),
            description: "Cancel (delete) a task by ID.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "Task ID to cancel" }
                },
                "required": ["task_id"]
            }),
        },
        AnthropicTool {
            name: "task_output".to_string(),
            description: "Get the output field of a task (set via task_update).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "Task ID" }
                },
                "required": ["task_id"]
            }),
        },
        AnthropicTool {
            name: "web_fetch".to_string(),
            description: "Fetch a URL and return its text content (HTML stripped). Truncated at 50 000 chars. Use for reading docs, RFCs, GitHub files, etc.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch" }
                },
                "required": ["url"]
            }),
        },
        AnthropicTool {
            name: "web_search".to_string(),
            description: "Search the web via Brave Search. Requires BRAVE_API_KEY env var. Returns up to 10 results with title, URL, and description.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" }
                },
                "required": ["query"]
            }),
        },
    ]
}

// ── Anthropic Streaming ───────────────────────────────────────────────────────

struct StreamUsage {
    input_tokens: u64,
    output_tokens: u64,
}

enum PartialBlock {
    Text { text: String },
    ToolUse { id: String, name: String, partial_json: String },
}

/// Calls the Anthropic Messages API with streaming and returns (content_blocks, stop_reason, usage).
/// Emits ChatEvent::Text and ChatEvent::ToolUse to the frontend as they arrive.
async fn stream_turn(
    app: &AppHandle,
    session_id: &str,
    messages: &[ApiMessage],
    system: &str,
    model: &str,
    api_key: &str,
    aborted: &AtomicBool,
) -> Result<(Vec<ContentBlock>, String, StreamUsage), String> {
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 128000,
        "system": system,
        "tools": tool_definitions(),
        "messages": messages,
        "stream": true,
    });

    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("API error {status}: {text}"));
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();

    // Block being accumulated, indexed by block index
    let mut partial: HashMap<usize, PartialBlock> = HashMap::new();
    let mut completed: Vec<(usize, ContentBlock)> = Vec::new();

    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut stop_reason = "end_turn".to_string();

    while let Some(chunk) = stream.next().await {
        if aborted.load(Ordering::Relaxed) {
            return Err("__interrupted__".to_string());
        }

        let bytes = chunk.map_err(|e| e.to_string())?;
        buf.push_str(&String::from_utf8_lossy(&bytes));

        // Process complete lines
        loop {
            let Some(nl) = buf.find('\n') else { break };
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf = buf[nl + 1..].to_string();

            if !line.starts_with("data: ") {
                continue;
            }
            let json_str = &line[6..];
            if json_str == "[DONE]" {
                break;
            }

            let Ok(ev) = serde_json::from_str::<serde_json::Value>(json_str) else {
                continue;
            };

            match ev["type"].as_str().unwrap_or("") {
                "message_start" => {
                    if let Some(u) = ev["message"]["usage"]["input_tokens"].as_u64() {
                        input_tokens = u;
                    }
                }
                "content_block_start" => {
                    let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                    match ev["content_block"]["type"].as_str().unwrap_or("") {
                        "text" => {
                            partial.insert(idx, PartialBlock::Text { text: String::new() });
                        }
                        "tool_use" => {
                            let id = ev["content_block"]["id"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            let name = ev["content_block"]["name"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            partial.insert(
                                idx,
                                PartialBlock::ToolUse { id, name, partial_json: String::new() },
                            );
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
                                if let Some(PartialBlock::Text { text }) =
                                    partial.get_mut(&idx)
                                {
                                    text.push_str(delta);
                                }
                                emit(app, session_id, ChatEvent::Text {
                                    text: delta.to_string(),
                                });
                            }
                        }
                        "input_json_delta" => {
                            let delta = ev["delta"]["partial_json"].as_str().unwrap_or("");
                            if let Some(PartialBlock::ToolUse { partial_json, .. }) =
                                partial.get_mut(&idx)
                            {
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
                                let input: serde_json::Value =
                                    serde_json::from_str(&partial_json)
                                        .unwrap_or(serde_json::Value::Object(
                                            serde_json::Map::new(),
                                        ));
                                emit(app, session_id, ChatEvent::ToolUse {
                                    tool: name.clone(),
                                    input: input.clone(),
                                });
                                completed.push((
                                    idx,
                                    ContentBlock::ToolUse { id, name, input },
                                ));
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

    // Sort by index and return
    completed.sort_by_key(|(i, _)| *i);
    let blocks: Vec<ContentBlock> = completed.into_iter().map(|(_, b)| b).collect();

    Ok((blocks, stop_reason, StreamUsage { input_tokens, output_tokens }))
}

// ── Agentic Loop ──────────────────────────────────────────────────────────────

async fn run_agentic_loop(
    app: AppHandle,
    session: Arc<Mutex<Session>>,
    session_id: String,
    api_key: String,
    model: String,
) {
    let mut turns = 0usize;
    let mut total_input = 0u64;
    let mut total_output = 0u64;

    loop {
        let (messages, system, cwd, aborted, pending_question) = {
            let s = session.lock().unwrap();
            (s.messages.clone(), s.system_prompt.clone(), s.cwd.clone(), s.aborted.clone(), s.pending_question.clone())
        };

        if aborted.load(Ordering::Relaxed) {
            emit(&app, &session_id, ChatEvent::Interrupted);
            return;
        }

        match stream_turn(
            &app,
            &session_id,
            &messages,
            &system,
            &model,
            &api_key,
            &aborted,
        )
        .await
        {
            Err(e) if e == "__interrupted__" => {
                emit(&app, &session_id, ChatEvent::Interrupted);
                return;
            }
            Err(e) => {
                emit(&app, &session_id, ChatEvent::Error { message: e });
                return;
            }
            Ok((blocks, stop_reason, usage)) => {
                turns += 1;
                total_input += usage.input_tokens;
                total_output += usage.output_tokens;

                // Add assistant turn to history
                {
                    let mut s = session.lock().unwrap();
                    s.messages.push(ApiMessage {
                        role: "assistant".to_string(),
                        content: blocks.clone(),
                    });
                }

                if stop_reason != "tool_use" {
                    // Done
                    let cost = cost_usd(&model, total_input, total_output);
                    emit(&app, &session_id, ChatEvent::Result {
                        cost_usd: cost,
                        turns,
                        session_id: session_id.clone(),
                        result: None,
                    });
                    return;
                }

                // Execute tools and add results
                let mut tool_results: Vec<ContentBlock> = Vec::new();
                for block in &blocks {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let result = execute_tool(name, input, &cwd, &app, &session_id, pending_question.clone()).await;
                        emit(&app, &session_id, ChatEvent::ToolResult {
                            tool_use_id: id.clone(),
                            content: serde_json::Value::String(result.clone()),
                        });
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: vec![serde_json::json!({"type": "text", "text": result})],
                        });
                    }
                }

                {
                    let mut s = session.lock().unwrap();
                    s.messages.push(ApiMessage {
                        role: "user".to_string(),
                        content: tool_results,
                    });
                }
            }
        }
    }
}

fn cost_usd(model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
    // Approximate pricing per MTok
    let (input_rate, output_rate) = if model.contains("opus") {
        (15.0, 75.0)
    } else if model.contains("sonnet") {
        (3.0, 15.0)
    } else {
        (3.0, 15.0)
    };
    (input_tokens as f64 * input_rate + output_tokens as f64 * output_rate) / 1_000_000.0
}

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

// ── Path Completion ───────────────────────────────────────────────────────────

/// Given one or more root directories, a directory fragment, and a filename prefix,
/// return all matching paths (relative to the root) in the form `dir_part + name[/]`.
/// Searches all roots and deduplicates.
#[tauri::command]
fn get_completions(roots: Vec<String>, dir_part: String, file_part: String) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();

    for root in &roots {
        let search_dir = PathBuf::from(root).join(&dir_part);
        let Ok(entries) = fs::read_dir(&search_dir) else { continue };

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip hidden files unless the prefix starts with '.'
            if name.starts_with('.') && !file_part.starts_with('.') {
                continue;
            }
            if !name.to_lowercase().starts_with(&file_part.to_lowercase()) {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let completion = if is_dir {
                format!("{}{}/", dir_part, name)
            } else {
                format!("{}{}", dir_part, name)
            };
            if seen.insert(completion.clone()) {
                results.push(completion);
            }
        }
    }

    results.sort();
    results
}

// ── Tauri Commands ────────────────────────────────────────────────────────────

#[tauri::command]
fn get_repo() -> Option<String> {
    read_config().repo
}

#[tauri::command]
fn set_repo(repo: String) {
    let mut cfg = read_config();
    cfg.repo = Some(repo);
    write_config(&cfg);
}

/// Parse ANTHROPIC_API_KEY from shell dotfiles, for GUI apps that don't inherit env vars.
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

/// Resolve API key: environment variable takes precedence over stored config,
/// falling back to dotfile parsing (for GUI apps launched outside a terminal).
fn resolve_api_key() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty())
        .or_else(|| read_config().api_key)
        .or_else(|| read_key_from_shell_files())
}

#[tauri::command]
fn get_api_key() -> Option<String> {
    resolve_api_key()
}

#[tauri::command]
fn set_api_key(key: String) {
    let mut cfg = read_config();
    cfg.api_key = Some(key);
    write_config(&cfg);
}

#[tauri::command]
fn chat_new_session(
    state: tauri::State<'_, AppState>,
    app: AppHandle,
    _session_type: String, // "main" or "worker" — reserved for future routing
    branch: Option<String>,
    worktree_path: Option<String>,
    repo: String,
) -> String {
    let session_id = Uuid::new_v4().to_string();
    let cwd = worktree_path.clone().unwrap_or_else(|| repo.clone());
    let system_prompt = build_system_prompt(
        &repo,
        branch.as_deref(),
        worktree_path.as_deref(),
    );

    let session = Arc::new(Mutex::new(Session {
        messages: Vec::new(),
        system_prompt,
        cwd,
        aborted: Arc::new(AtomicBool::new(false)),
        pending_question: Arc::new(tokio::sync::Mutex::new(None)),
    }));

    state.sessions.lock().unwrap().insert(session_id.clone(), session);

    // Emit ready event asynchronously so the frontend has time to register the listener
    let sid = session_id.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        emit(&app, &sid, ChatEvent::Ready { session_id: sid.clone(), resumed: false });
    });

    session_id
}

#[tauri::command]
async fn chat_send(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    session_id: String,
    text: String,
) -> Result<(), String> {
    let session = {
        state.sessions.lock().unwrap().get(&session_id).cloned()
    };
    let session = session.ok_or_else(|| "session not found".to_string())?;

    let config = read_config();
    let api_key = resolve_api_key().ok_or_else(|| "no API key configured".to_string())?;
    let model = config.model.unwrap_or_else(|| "claude-sonnet-4-6".to_string());

    // Reset abort flag and add user message
    {
        let mut s = session.lock().unwrap();
        s.aborted.store(false, Ordering::Relaxed);
        s.messages.push(ApiMessage {
            role: "user".to_string(),
            content: vec![ContentBlock::Text { text }],
        });
    }

    run_agentic_loop(app, session, session_id, api_key, model).await;
    Ok(())
}

#[tauri::command]
async fn chat_answer(
    state: tauri::State<'_, AppState>,
    session_id: String,
    answer: String,
) -> Result<(), String> {
    let session = {
        state.sessions.lock().unwrap().get(&session_id).cloned()
    };
    let session = session.ok_or_else(|| "session not found".to_string())?;
    let pending_question = session.lock().unwrap().pending_question.clone();
    let mut slot = pending_question.lock().await;
    match slot.take() {
        Some(tx) => { tx.send(answer).ok(); Ok(()) }
        None => Err("no pending question".to_string()),
    }
}

#[tauri::command]
fn chat_interrupt(
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    let sessions = state.sessions.lock().unwrap();
    if let Some(session) = sessions.get(&session_id) {
        session.lock().unwrap().aborted.store(true, Ordering::Relaxed);
        Ok(())
    } else {
        Err("session not found".to_string())
    }
}

#[tauri::command]
async fn spawn_worker(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    session_id: String,
    task: String,
    repo: String,
) -> Result<(), String> {
    emit(&app, &session_id, ChatEvent::Spawning { task: task.clone() });

    // Generate branch name via Claude (Haiku, fast + cheap), fall back to slug
    let api_key = resolve_api_key().unwrap_or_default();
    let branch = generate_branch_name(&task, &api_key).await;
    let branch = if branch.is_empty() { Uuid::new_v4().to_string()[..8].to_string() } else { branch };

    // Create git worktree
    let worktree_path = match create_worktree(&repo, &branch) {
        Ok(p) => p,
        Err(e) => {
            emit(&app, &session_id, ChatEvent::WorkerError { message: e });
            return Ok(());
        }
    };

    emit(&app, &session_id, ChatEvent::WorkerCreated {
        branch: branch.clone(),
        worktree_path: worktree_path.clone(),
        task: task.clone(),
    });

    // Create worker session
    let worker_session_id = Uuid::new_v4().to_string();
    let system_prompt = build_system_prompt(&repo, Some(&branch), Some(&worktree_path));
    let worker_session = Arc::new(Mutex::new(Session {
        messages: Vec::new(),
        system_prompt,
        cwd: worktree_path.clone(),
        aborted: Arc::new(AtomicBool::new(false)),
        pending_question: Arc::new(tokio::sync::Mutex::new(None)),
    }));

    state
        .sessions
        .lock()
        .unwrap()
        .insert(worker_session_id.clone(), worker_session.clone());

    // Emit worker session ready — frontend will use branch as the session key
    // (worker tabs look up session by branch name via a separate mapping held in the frontend)
    let app2 = app.clone();
    let wsid = worker_session_id.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        emit(&app2, &wsid, ChatEvent::Ready { session_id: wsid.clone(), resumed: false });
    });

    // Notify the parent chat that the worker is ready, including the worker's session_id
    emit(&app, &session_id, ChatEvent::WorkerSessionReady {
        branch,
        worktree_path: worktree_path.clone(),
        worker_session_id,
        task,
    });

    Ok(())
}

// ── App Setup ─────────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            sessions: Mutex::new(HashMap::new()),
        })
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_repo,
            set_repo,
            get_api_key,
            set_api_key,
            get_branches,
            get_completions,
            chat_new_session,
            chat_send,
            chat_answer,
            chat_interrupt,
            spawn_worker,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
