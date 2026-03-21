use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
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
use claudulhu_core::{
    build_system_prompt, create_worktree, effective_repo, generate_branch_name,
    get_branches_for_repo, init_shell_env, read_config, resolve_api_key, run_agentic_loop,
    write_config, ApiMessage, ChatEvent, Config, ContentBlock, Session,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

// ── App State ─────────────────────────────────────────────────────────────────

struct AppState {
    /// Active sessions keyed by session_id
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
    /// Worker sessions created by spawn_worker, waiting for WS connection (keyed by branch)
    worker_sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
}

// ── Client Messages (client → server, WebSocket) ──────────────────────────────

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Message     { text: String },
    Interrupt,
    SpawnWorker { task: String },
    Answer      { answer: String },
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
        aborted:          Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pending_question: Arc::new(tokio::sync::Mutex::new(None)),
    }));

    app_state.worker_sessions.lock().unwrap().insert(branch.clone(), worker_session.clone());

    tx.send(ChatEvent::WorkerSessionReady {
        branch:            branch.clone(),
        worktree_path:     worktree_path.clone(),
        worker_session_id: worker_session_id.clone(),
        task:              task.to_string(),
    }).await.ok();

    app_state.sessions.lock().unwrap().insert(worker_session_id, worker_session);
}

// ── WebSocket Session Handler ─────────────────────────────────────────────────

async fn run_session(socket: WebSocket, session: Arc<Mutex<Session>>, session_id: String, app_state: Arc<AppState>, repo: String) {
    let (mut ws_sink, mut ws_stream) = socket.split();

    let (tx, mut rx) = mpsc::channel::<ChatEvent>(256);

    let send_task = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let json = match serde_json::to_string(&event) {
                Ok(s)  => s,
                Err(_) => continue,
            };
            if ws_sink.send(Message::Text(json)).await.is_err() { break; }
        }
    });

    tx.send(ChatEvent::Ready { session_id: session_id.clone(), resumed: false }).await.ok();

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

    session.lock().unwrap().aborted.store(true, Ordering::Relaxed);
    send_task.abort();
}

// ── HTTP Handlers ─────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn get_branches_handler(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let cfg  = read_config();
    let repo = effective_repo(&cfg);
    match get_branches_for_repo(&repo) {
        Ok(branches) => Json(branches).into_response(),
        Err(e)       => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_config_handler() -> Json<Config> {
    Json(read_config())
}

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
    ws:           WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
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
            aborted:          Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_question: Arc::new(tokio::sync::Mutex::new(None)),
        }));
        state.sessions.lock().unwrap().insert(session_id.clone(), session.clone());
        println!("[{}] /chat connected (repo: {})", session_id, repo);

        run_session(socket, session, session_id.clone(), state.clone(), repo).await;

        state.sessions.lock().unwrap().remove(&session_id);
        println!("[{}] /chat disconnected", session_id);
    })
}

async fn worker_ws_handler(
    ws:           WebSocketUpgrade,
    Path(branch): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let existing = state.worker_sessions.lock().unwrap().remove(&branch);

        let (session, repo) = if let Some(sess) = existing {
            let cwd = sess.lock().unwrap().cwd.clone();
            (sess, cwd)
        } else {
            let cfg    = read_config();
            let repo   = effective_repo(&cfg);
            let system = build_system_prompt(&repo, Some(&branch), None);
            let sess   = Arc::new(Mutex::new(Session {
                messages:         Vec::new(),
                system_prompt:    system,
                cwd:              repo.clone(),
                aborted:          Arc::new(std::sync::atomic::AtomicBool::new(false)),
                pending_question: Arc::new(tokio::sync::Mutex::new(None)),
            }));
            (sess, repo)
        };

        let session_id = Uuid::new_v4().to_string();
        state.sessions.lock().unwrap().insert(session_id.clone(), session.clone());
        println!("[{}] /workers/{} connected (repo: {})", session_id, branch, repo);

        run_session(socket, session, session_id.clone(), state.clone(), repo).await;

        state.sessions.lock().unwrap().remove(&session_id);
        println!("[{}] /workers/{} disconnected", session_id, branch);
    })
}

// ── Main ──────────────────────────────────────────────────────────────────────

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
        .route("/health",          get(health_handler))
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
    println!("  HTTP GET:  http://{addr}/health");
    println!("  HTTP GET:  http://{addr}/branches");
    println!("  HTTP GET:  http://{addr}/config");
    println!("  HTTP PUT:  http://{addr}/config");

    axum::serve(listener, app).await.unwrap();
}
