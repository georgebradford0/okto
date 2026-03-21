use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use claudulhu_core::{
    build_system_prompt, create_worktree, generate_branch_name, get_branches_for_repo,
    init_shell_env, read_config, resolve_api_key, run_agentic_loop, write_config,
    ApiMessage, Branch, ChatEvent, ContentBlock, Session,
};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use uuid::Uuid;

// ── App State ─────────────────────────────────────────────────────────────────

struct AppState {
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
}

// ── Event Bridge ──────────────────────────────────────────────────────────────

fn emit(app: &AppHandle, session_id: &str, event: ChatEvent) {
    let channel = format!("claude-event-{session_id}");
    app.emit(&channel, event).ok();
}

/// Wraps core's run_agentic_loop with an mpsc→AppHandle bridge so Tauri
/// event emission stays in the desktop crate.
async fn run_agentic_loop_desktop(
    app:        AppHandle,
    session:    Arc<Mutex<Session>>,
    session_id: String,
    api_key:    String,
    model:      String,
) {
    let (tx, mut rx) = mpsc::channel::<ChatEvent>(256);
    let app2 = app.clone();
    let sid2 = session_id.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            emit(&app2, &sid2, event);
        }
    });
    run_agentic_loop(session, session_id, api_key, model, tx).await;
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
    resolve_api_key()
}

#[tauri::command]
fn set_api_key(key: String) {
    let mut cfg = read_config();
    cfg.api_key = Some(key);
    write_config(&cfg);
}

#[tauri::command]
fn get_branches(repo: String) -> Result<Vec<Branch>, String> {
    get_branches_for_repo(&repo)
}

#[tauri::command]
fn chat_new_session(
    state: tauri::State<'_, AppState>,
    app: AppHandle,
    _session_type: String,
    branch: Option<String>,
    worktree_path: Option<String>,
    repo: String,
) -> String {
    let session_id    = Uuid::new_v4().to_string();
    let cwd           = worktree_path.clone().unwrap_or_else(|| repo.clone());
    let system_prompt = build_system_prompt(&repo, branch.as_deref(), worktree_path.as_deref());

    let session = Arc::new(Mutex::new(Session {
        messages: Vec::new(),
        system_prompt,
        cwd,
        aborted:          Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pending_question: Arc::new(tokio::sync::Mutex::new(None)),
    }));

    state.sessions.lock().unwrap().insert(session_id.clone(), session);

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

    let cfg     = read_config();
    let api_key = resolve_api_key().ok_or_else(|| "no API key configured".to_string())?;
    let model   = cfg.model.unwrap_or_else(|| "claude-sonnet-4-6".to_string());

    {
        let mut s = session.lock().unwrap();
        s.aborted.store(false, Ordering::Relaxed);
        s.messages.push(ApiMessage {
            role:    "user".to_string(),
            content: vec![ContentBlock::Text { text }],
        });
    }

    run_agentic_loop_desktop(app, session, session_id, api_key, model).await;
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
        None     => Err("no pending question".to_string()),
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

    let api_key = resolve_api_key().unwrap_or_default();
    let branch  = generate_branch_name(&task, &api_key).await;
    let branch  = if branch.is_empty() { Uuid::new_v4().to_string()[..8].to_string() } else { branch };

    let worktree_path = match create_worktree(&repo, &branch) {
        Ok(p)  => p,
        Err(e) => { emit(&app, &session_id, ChatEvent::WorkerError { message: e }); return Ok(()); }
    };

    emit(&app, &session_id, ChatEvent::WorkerCreated {
        branch: branch.clone(),
        worktree_path: worktree_path.clone(),
        task: task.clone(),
    });

    let worker_session_id = Uuid::new_v4().to_string();
    let system_prompt     = build_system_prompt(&repo, Some(&branch), Some(&worktree_path));
    let worker_session    = Arc::new(Mutex::new(Session {
        messages:         Vec::new(),
        system_prompt,
        cwd:              worktree_path.clone(),
        aborted:          Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pending_question: Arc::new(tokio::sync::Mutex::new(None)),
    }));

    state.sessions.lock().unwrap().insert(worker_session_id.clone(), worker_session.clone());

    let app2 = app.clone();
    let wsid = worker_session_id.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        emit(&app2, &wsid, ChatEvent::Ready { session_id: wsid.clone(), resumed: false });
    });

    emit(&app, &session_id, ChatEvent::WorkerSessionReady {
        branch,
        worktree_path,
        worker_session_id,
        task,
    });

    Ok(())
}

// ── Path Completion ───────────────────────────────────────────────────────────

#[tauri::command]
fn get_completions(roots: Vec<String>, dir_part: String, file_part: String) -> Vec<String> {
    let mut seen    = std::collections::HashSet::new();
    let mut results = Vec::new();

    for root in &roots {
        let search_dir = PathBuf::from(root).join(&dir_part);
        let Ok(entries) = fs::read_dir(&search_dir) else { continue };

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && !file_part.starts_with('.') { continue; }
            if !name.to_lowercase().starts_with(&file_part.to_lowercase()) { continue; }
            let is_dir     = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
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

// ── App Setup ─────────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_shell_env();
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
