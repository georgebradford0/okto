use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

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

// ── Tool Execution ────────────────────────────────────────────────────────────

async fn execute_tool(name: &str, input: &serde_json::Value, cwd: &str) -> String {
    match name {
        "bash" => {
            let cmd = input["command"].as_str().unwrap_or("");
            match tokio::process::Command::new("bash")
                .arg("-c")
                .arg(cmd)
                .current_dir(cwd)
                .output()
                .await
            {
                Ok(o) => {
                    let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                    if stderr.is_empty() {
                        stdout
                    } else {
                        format!("{stdout}\n[stderr]: {stderr}")
                    }
                }
                Err(e) => format!("error: {e}"),
            }
        }
        "read_file" => {
            let p = input["path"].as_str().unwrap_or("");
            let full = if p.starts_with('/') {
                PathBuf::from(p)
            } else {
                PathBuf::from(cwd).join(p)
            };
            fs::read_to_string(&full).unwrap_or_else(|e| format!("error: {e}"))
        }
        "write_file" => {
            let p = input["path"].as_str().unwrap_or("");
            let content = input["content"].as_str().unwrap_or("");
            let full = if p.starts_with('/') {
                PathBuf::from(p)
            } else {
                PathBuf::from(cwd).join(p)
            };
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).ok();
            }
            match fs::write(&full, content) {
                Ok(_) => "ok".to_string(),
                Err(e) => format!("error: {e}"),
            }
        }
        "glob" => {
            let pattern = input["pattern"].as_str().unwrap_or("**/*");
            let base = PathBuf::from(cwd);
            let full_pattern = format!("{cwd}/{pattern}");
            match glob::glob(&full_pattern) {
                Ok(paths) => paths
                    .filter_map(|p| p.ok())
                    .filter(|p| p.is_file())
                    .map(|p| {
                        p.strip_prefix(&base)
                            .map(|r| r.to_string_lossy().to_string())
                            .unwrap_or_else(|_| p.to_string_lossy().to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                Err(e) => format!("error: {e}"),
            }
        }
        "grep" => {
            let pattern = input["pattern"].as_str().unwrap_or("");
            let path = input["path"].as_str().unwrap_or(".");
            match tokio::process::Command::new("grep")
                .args(["-r", "-n", pattern, path])
                .current_dir(cwd)
                .output()
                .await
            {
                Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
                Err(e) => format!("error: {e}"),
            }
        }
        _ => format!("unknown tool: {name}"),
    }
}

fn tool_definitions() -> Vec<AnthropicTool> {
    vec![
        AnthropicTool {
            name: "bash".to_string(),
            description: "Run a shell command in the repository directory. Returns stdout/stderr."
                .to_string(),
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
            description: "Read the contents of a file.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path relative to repo or absolute" }
                },
                "required": ["path"]
            }),
        },
        AnthropicTool {
            name: "write_file".to_string(),
            description: "Write content to a file, creating parent directories as needed."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
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
            description: "Search file contents for a regex pattern.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern" },
                    "path": { "type": "string", "description": "Directory or file to search (default: .)" }
                },
                "required": ["pattern"]
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
        "max_tokens": 8096,
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
        let (messages, system, cwd, aborted) = {
            let s = session.lock().unwrap();
            (s.messages.clone(), s.system_prompt.clone(), s.cwd.clone(), s.aborted.clone())
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
                        let result = execute_tool(name, input, &cwd).await;
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
    match (branch, worktree_path) {
        (Some(branch), Some(wt)) => format!(
            "You are an AI coding assistant working on branch '{branch}' of the git repository at {repo_path}.\n\
             Your working directory is the worktree at {wt}.\n\
             Use the bash, read_file, write_file, glob, and grep tools to inspect and modify the codebase.\n\
             Be concise and precise."
        ),
        _ => format!(
            "You are an AI assistant helping manage the git repository at {repo_path}.\n\
             You can inspect code, answer questions, and help coordinate work across branches.\n\
             Use the bash, read_file, glob, and grep tools to explore the codebase.\n\
             Be concise and precise."
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

#[tauri::command]
fn get_api_key() -> Option<String> {
    read_config().api_key
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
    let api_key = config.api_key.ok_or_else(|| "no API key configured".to_string())?;
    let model = config.model.unwrap_or_else(|| "claude-opus-4-6".to_string());

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

    // Generate branch name from task
    let branch = slug(&task);
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
        cwd: worktree_path,
        aborted: Arc::new(AtomicBool::new(false)),
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
            chat_interrupt,
            spawn_worker,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
