use std::{
    collections::HashMap,
    fs,
    io::Write as _,
    path::PathBuf,
    sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex},
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
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
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, Notify},
};
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

// ── Noise Protocol ────────────────────────────────────────────────────────────

const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
const NOISE_KEY_FILE: &str = "/etc/claudulhu/noise_key.bin";

fn to_base32(data: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in data {
        buf = (buf << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHA[((buf >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHA[((buf << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// Load a 64-byte keypair (32 private + 32 public) from disk, generating it if absent.
fn load_or_generate_keypair(path: &str) -> (Vec<u8>, Vec<u8>) {
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.len() == 64 {
            return (bytes[..32].to_vec(), bytes[32..].to_vec());
        }
    }
    let builder = snow::Builder::new(NOISE_PATTERN.parse().expect("valid pattern"));
    let kp = builder.generate_keypair().expect("keygen");
    let mut combined = kp.private.clone();
    combined.extend_from_slice(&kp.public);
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, &combined).ok();
    (kp.private, kp.public)
}

/// Read a framed Noise message: [u16 BE length][payload].
async fn read_noise_frame(stream: &mut tokio::net::TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write a framed Noise message: [u16 BE length][payload].
async fn write_noise_frame(stream: &mut tokio::net::TcpStream, data: &[u8]) -> anyhow::Result<()> {
    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(data).await?;
    Ok(())
}

/// Perform the Noise_XX responder handshake.
async fn noise_handshake(
    stream: &mut tokio::net::TcpStream,
    static_private: &[u8],
) -> anyhow::Result<snow::TransportState> {
    let builder = snow::Builder::new(NOISE_PATTERN.parse()?);
    let mut hs = builder.local_private_key(static_private).build_responder()?;

    let mut payload = vec![0u8; 65535];

    // Message 1: ← e
    let msg1 = read_noise_frame(stream).await?;
    eprintln!("[noise-dbg] msg1 ({} bytes): {}", msg1.len(), hex::encode(&msg1));
    hs.read_message(&msg1, &mut payload)?;

    // Message 2: → e, ee, s, es
    let mut msg2 = vec![0u8; 65535];
    let n = hs.write_message(&[], &mut msg2)?;
    eprintln!("[noise-dbg] msg2 ({} bytes): {}", n, hex::encode(&msg2[..n]));
    write_noise_frame(stream, &msg2[..n]).await?;

    // Message 3: ← s, se
    let msg3 = read_noise_frame(stream).await?;
    eprintln!("[noise-dbg] msg3 ({} bytes): {}", msg3.len(), hex::encode(&msg3));
    hs.read_message(&msg3, &mut payload)?;

    Ok(hs.into_transport_mode()?)
}

/// Proxy a Noise transport connection to the local HTTP/WebSocket server.
async fn handle_noise_connection(
    mut stream: tokio::net::TcpStream,
    static_private: Arc<Vec<u8>>,
    http_port: u16,
) -> anyhow::Result<()> {
    let transport = noise_handshake(&mut stream, &static_private).await?;
    let transport = Arc::new(Mutex::new(transport));

    let local = tokio::net::TcpStream::connect(format!("127.0.0.1:{http_port}")).await?;

    let (mut raw_read, mut raw_write) = stream.into_split();
    let (mut local_read, mut local_write) = local.into_split();

    let transport_enc = transport.clone();
    let transport_dec = transport.clone();

    // Task A: local → encrypt → raw
    let task_a = tokio::spawn(async move {
        let mut plain = vec![0u8; 65000];
        let mut enc = vec![0u8; 65535];
        loop {
            let n = local_read.read(&mut plain).await.unwrap_or(0);
            if n == 0 { break; }
            let enc_n = match transport_enc.lock().unwrap().write_message(&plain[..n], &mut enc) {
                Ok(n) => n,
                Err(_) => break,
            };
            let len = (enc_n as u16).to_be_bytes();
            if raw_write.write_all(&len).await.is_err() { break; }
            if raw_write.write_all(&enc[..enc_n]).await.is_err() { break; }
        }
    });

    // Task B: raw → decrypt → local
    let task_b = tokio::spawn(async move {
        let mut len_buf = [0u8; 2];
        let mut enc = vec![0u8; 65535];
        let mut dec = vec![0u8; 65535];
        loop {
            if raw_read.read_exact(&mut len_buf).await.is_err() { break; }
            let len = u16::from_be_bytes(len_buf) as usize;
            if len > enc.len() { break; }
            if raw_read.read_exact(&mut enc[..len]).await.is_err() { break; }
            let dec_n = match transport_dec.lock().unwrap().read_message(&enc[..len], &mut dec) {
                Ok(n) => n,
                Err(_) => break,
            };
            if local_write.write_all(&dec[..dec_n]).await.is_err() { break; }
        }
    });

    tokio::select! {
        _ = task_a => {}
        _ = task_b => {}
    }
    Ok(())
}

/// Run the Noise TCP proxy listener.
async fn run_noise_proxy(static_private: Vec<u8>, noise_port: u16, http_port: u16) {
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{noise_port}"))
        .await
        .expect("failed to bind Noise port");
    println!("[noise] listening on 0.0.0.0:{noise_port} → 127.0.0.1:{http_port}");

    let static_private = Arc::new(static_private);
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        println!("[noise] connection from {peer}");
        let priv_clone = static_private.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_noise_connection(stream, priv_clone, http_port).await {
                eprintln!("[noise] connection error from {peer}: {e}");
            }
        });
    }
}

// ── Session Persistence ───────────────────────────────────────────────────────

fn sessions_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".claudulhu")
        .join("sessions")
}

fn session_dir(session_id: &str) -> PathBuf {
    sessions_dir().join(session_id)
}

fn persist_meta(session_id: &str, cwd: &str, system_prompt: &str) {
    let dir = session_dir(session_id);
    fs::create_dir_all(&dir).ok();
    let meta = serde_json::json!({ "cwd": cwd, "system_prompt": system_prompt });
    fs::write(dir.join("meta.json"), meta.to_string()).ok();
}

fn persist_messages(session_id: &str, messages: &[ApiMessage]) {
    let dir = session_dir(session_id);
    fs::create_dir_all(&dir).ok();
    if let Ok(json) = serde_json::to_string(messages) {
        fs::write(dir.join("messages.json"), json).ok();
    }
}

fn append_event_to_disk(session_id: &str, event: &ChatEvent) {
    let dir = session_dir(session_id);
    fs::create_dir_all(&dir).ok();
    if let Ok(json) = serde_json::to_string(event) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true).append(true)
            .open(dir.join("events.jsonl"))
        {
            let _ = writeln!(f, "{}", json);
        }
    }
}

/// Load a persisted session from disk. Returns (messages, events, cwd, system_prompt).
fn load_session_from_disk(session_id: &str) -> Option<(Vec<ApiMessage>, Vec<ChatEvent>, String, String)> {
    let dir = session_dir(session_id);
    if !dir.exists() { return None; }

    let meta: serde_json::Value = fs::read_to_string(dir.join("meta.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let cwd           = meta["cwd"].as_str().unwrap_or_default().to_string();
    let system_prompt = meta["system_prompt"].as_str().unwrap_or_default().to_string();

    let messages: Vec<ApiMessage> = fs::read_to_string(dir.join("messages.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let events: Vec<ChatEvent> = fs::read_to_string(dir.join("events.jsonl"))
        .ok()
        .map(|s| {
            s.lines()
                .filter_map(|l| serde_json::from_str::<ChatEvent>(l).ok())
                .collect()
        })
        .unwrap_or_default();

    if cwd.is_empty() { return None; }
    Some((messages, events, cwd, system_prompt))
}

// ── Active Session ────────────────────────────────────────────────────────────

/// A session that persists across WebSocket disconnects.
/// The agentic loop runs independently; events accumulate and are replayed on reconnect.
struct ActiveSession {
    id:           String,
    inner:        Arc<Mutex<Session>>,
    /// Append-only log of all ChatEvents produced for this session.
    events:       Arc<Mutex<Vec<ChatEvent>>>,
    /// Notified (via notify_one) whenever a new event is appended.
    notify:       Arc<Notify>,
    /// Sender end of the accumulator channel. Kept alive to keep the accumulator task running.
    /// The agentic loop clones this to send events.
    loop_tx:      mpsc::Sender<ChatEvent>,
    /// True while the agentic loop task is running.
    loop_running: Arc<AtomicBool>,
}

fn new_active_session(
    id:            String,
    system_prompt: String,
    cwd:           String,
) -> Arc<ActiveSession> {
    new_active_session_with_data(id, system_prompt, cwd, Vec::new(), Vec::new())
}

fn new_active_session_with_data(
    id:             String,
    system_prompt:  String,
    cwd:            String,
    messages:       Vec<ApiMessage>,
    existing_events: Vec<ChatEvent>,
) -> Arc<ActiveSession> {
    let events = Arc::new(Mutex::new(existing_events));
    let notify = Arc::new(Notify::new());
    let (loop_tx, mut loop_rx) = mpsc::channel::<ChatEvent>(256);

    let events_c = events.clone();
    let notify_c = notify.clone();
    let id_c     = id.clone();
    tokio::spawn(async move {
        while let Some(event) = loop_rx.recv().await {
            append_event_to_disk(&id_c, &event);
            events_c.lock().unwrap().push(event);
            notify_c.notify_one();
        }
    });

    let inner = Arc::new(Mutex::new(Session {
        messages,
        system_prompt,
        cwd,
        aborted:          Arc::new(AtomicBool::new(false)),
        pending_question: Arc::new(tokio::sync::Mutex::new(None)),
    }));

    Arc::new(ActiveSession {
        id,
        inner,
        events,
        notify,
        loop_tx,
        loop_running: Arc::new(AtomicBool::new(false)),
    })
}

// ── App State ─────────────────────────────────────────────────────────────────

struct AppState {
    sessions:        Mutex<HashMap<String, Arc<ActiveSession>>>,
    worker_sessions: Mutex<HashMap<String, Arc<ActiveSession>>>,
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

// ── URL Query Params ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct ChatQuery {
    /// Client provides the session_id it received in a previous `ready` frame.
    session_id: Option<String>,
    /// Last event seq the client successfully received (0-indexed count of events received).
    /// Server replays from this index onwards.
    seq: Option<usize>,
}

#[derive(Deserialize, Default)]
struct WorkerQuery {
    /// Last event seq the client successfully received for this worker session.
    seq: Option<usize>,
}

// ── Spawn Worker ──────────────────────────────────────────────────────────────

async fn spawn_worker(
    app_state: &Arc<AppState>,
    task:      &str,
    repo:      &str,
    tx:        mpsc::Sender<ChatEvent>,
) {
    tx.send(ChatEvent::Spawning { task: task.to_string() }).await.ok();

    let api_key = resolve_api_key().unwrap_or_default();
    let branch  = generate_branch_name(task, &api_key).await;
    let branch  = if branch.is_empty() { Uuid::new_v4().to_string()[..8].to_string() } else { branch };

    let worktree_path = match create_worktree(repo, &branch) {
        Ok(p)  => p,
        Err(e) => { tx.send(ChatEvent::WorkerError { message: e }).await.ok(); return; }
    };

    tx.send(ChatEvent::WorkerCreated {
        branch: branch.clone(),
        worktree_path: worktree_path.clone(),
        task: task.to_string(),
    }).await.ok();

    let system_prompt  = build_system_prompt(repo, Some(&branch), Some(&worktree_path));
    let worker_active  = new_active_session(branch.clone(), system_prompt, worktree_path.clone());

    app_state.worker_sessions.lock().unwrap().insert(branch.clone(), worker_active);

    let worker_session_id = Uuid::new_v4().to_string();
    tx.send(ChatEvent::WorkerSessionReady {
        branch:            branch.clone(),
        worktree_path:     worktree_path.clone(),
        worker_session_id,
        task:              task.to_string(),
    }).await.ok();
}

// ── Delivery Task ─────────────────────────────────────────────────────────────

/// Streams events from the session's event log to the WebSocket sender channel,
/// starting at `start_idx` and following new events as they arrive.
async fn deliver_events(
    events:    Arc<Mutex<Vec<ChatEvent>>>,
    notify:    Arc<Notify>,
    ws_tx:     mpsc::Sender<String>,
    start_idx: usize,
) {
    let mut idx = start_idx;
    loop {
        // Drain all available events from idx onwards.
        loop {
            let event = events.lock().unwrap().get(idx).cloned();
            match event {
                Some(e) => {
                    let json = match serde_json::to_string(&e) {
                        Ok(s)  => s,
                        Err(_) => { idx += 1; continue; }
                    };
                    if ws_tx.send(json).await.is_err() { return; }
                    idx += 1;
                }
                None => break,
            }
        }
        // Wait for new events. notify_one() stores a permit so we never miss a signal.
        notify.notified().await;
    }
}

// ── WebSocket Session Handler ─────────────────────────────────────────────────

async fn run_session(
    socket:    WebSocket,
    active:    Arc<ActiveSession>,
    app_state: Arc<AppState>,
    repo:      String,
    start_idx: usize,
    resumed:   bool,
) {
    let (mut ws_sink, mut ws_stream) = socket.split();

    // Unbounded channel from delivery/direct sends → WebSocket writer task.
    let (ws_tx, mut ws_rx) = mpsc::channel::<String>(1024);

    // WebSocket writer task.
    let write_task = tokio::spawn(async move {
        while let Some(json) = ws_rx.recv().await {
            if ws_sink.send(Message::Text(json)).await.is_err() { break; }
        }
    });

    // Send `ready` directly (not stored in the event log).
    let ready = ChatEvent::Ready { session_id: active.id.clone(), resumed };
    if let Ok(json) = serde_json::to_string(&ready) {
        ws_tx.send(json).await.ok();
    }

    // Delivery task: replays historical events then streams new ones.
    let deliver_task = tokio::spawn(deliver_events(
        active.events.clone(),
        active.notify.clone(),
        ws_tx.clone(),
        start_idx,
    ));

    // Incoming message loop.
    while let Some(Ok(msg)) = ws_stream.next().await {
        let text = match msg {
            Message::Text(t)                     => t,
            Message::Close(_)                    => break,
            Message::Ping(_) | Message::Pong(_)  => continue,
            Message::Binary(_)                   => continue,
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
                    None    => {
                        active.loop_tx.send(ChatEvent::Error {
                            message: "no API key configured".to_string(),
                        }).await.ok();
                        continue;
                    }
                };
                let model = cfg.model.unwrap_or_else(|| "claude-sonnet-4-6".to_string());

                {
                    let mut s = active.inner.lock().unwrap();
                    s.aborted.store(false, Ordering::Relaxed);
                    s.messages.push(ApiMessage {
                        role:    "user".to_string(),
                        content: vec![ContentBlock::Text { text }],
                    });
                    persist_messages(&active.id, &s.messages);
                }

                // Start the agentic loop only if it isn't already running.
                if !active.loop_running.swap(true, Ordering::SeqCst) {
                    let session_c     = active.inner.clone();
                    let sid_c         = active.id.clone();
                    let tx_c          = active.loop_tx.clone();
                    let running_c     = active.loop_running.clone();
                    let inner_c       = active.inner.clone();
                    let id_c          = active.id.clone();
                    tokio::spawn(async move {
                        run_agentic_loop(session_c, sid_c, api_key, model, tx_c).await;
                        // Persist final message history when loop ends.
                        let msgs = inner_c.lock().unwrap().messages.clone();
                        persist_messages(&id_c, &msgs);
                        running_c.store(false, Ordering::Relaxed);
                    });
                } else {
                    eprintln!("[{}] warning: message received while loop already running", active.id);
                }
            }

            ClientMessage::Interrupt => {
                active.inner.lock().unwrap().aborted.store(true, Ordering::Relaxed);
            }

            ClientMessage::SpawnWorker { task } => {
                let state_c = app_state.clone();
                let tx_c    = active.loop_tx.clone();
                let repo_c  = repo.clone();
                tokio::spawn(async move {
                    spawn_worker(&state_c, &task, &repo_c, tx_c).await;
                });
            }

            ClientMessage::Answer { answer } => {
                let pq   = active.inner.lock().unwrap().pending_question.clone();
                let mut slot = pq.lock().await;
                if let Some(sender) = slot.take() { sender.send(answer).ok(); }
            }
        }
    }

    // WebSocket disconnected — clean up delivery but do NOT abort the agentic loop.
    deliver_task.abort();
    drop(ws_tx);
    let _ = write_task.await;
    println!("[{}] WebSocket disconnected (loop_running={})", active.id, active.loop_running.load(Ordering::Relaxed));
}

// ── HTTP Handlers ─────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

#[derive(Deserialize)]
struct CompletionQuery {
    dir_part:  Option<String>,
    file_part: Option<String>,
}

async fn get_completions_handler(Query(params): Query<CompletionQuery>) -> Json<Vec<String>> {
    let cfg      = read_config();
    let repo     = effective_repo(&cfg);
    let dir_part  = params.dir_part.unwrap_or_default();
    let file_part = params.file_part.unwrap_or_default();

    let mut seen    = std::collections::HashSet::new();
    let mut results = Vec::new();

    let search_dir = PathBuf::from(&repo).join(&dir_part);
    if let Ok(entries) = fs::read_dir(&search_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && !file_part.starts_with('.') { continue; }
            if !name.to_lowercase().starts_with(&file_part.to_lowercase()) { continue; }
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
    Json(results)
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
    Query(query): Query<ChatQuery>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let cfg  = read_config();
        let repo = effective_repo(&cfg);

        let (active, start_idx, resumed) = if let Some(sid) = query.session_id {
            // Try in-memory first.
            let existing = state.sessions.lock().unwrap().get(&sid).cloned();
            if let Some(sess) = existing {
                let idx = query.seq.unwrap_or(0);
                println!("[{}] /chat reconnect (seq={})", sid, idx);
                (sess, idx, true)
            } else if let Some((msgs, evs, cwd, sys)) = load_session_from_disk(&sid) {
                // Restore from disk.
                let active = new_active_session_with_data(sid.clone(), sys, cwd, msgs, evs);
                persist_meta(&sid, &active.inner.lock().unwrap().cwd, &active.inner.lock().unwrap().system_prompt);
                state.sessions.lock().unwrap().insert(sid.clone(), active.clone());
                let idx = query.seq.unwrap_or(0);
                println!("[{}] /chat restored from disk (seq={})", sid, idx);
                (active, idx, true)
            } else {
                // Session not found — create fresh.
                let new_id = Uuid::new_v4().to_string();
                let system = build_system_prompt(&repo, None, None);
                let active = new_active_session(new_id.clone(), system.clone(), repo.clone());
                persist_meta(&new_id, &repo, &system);
                state.sessions.lock().unwrap().insert(new_id.clone(), active.clone());
                println!("[{}] /chat new session (requested {} not found)", new_id, sid);
                (active, 0, false)
            }
        } else {
            // Brand-new session.
            let session_id = Uuid::new_v4().to_string();
            let system     = build_system_prompt(&repo, None, None);
            let active     = new_active_session(session_id.clone(), system.clone(), repo.clone());
            persist_meta(&session_id, &repo, &system);
            state.sessions.lock().unwrap().insert(session_id.clone(), active.clone());
            println!("[{}] /chat new session (repo: {})", session_id, repo);
            (active, 0, false)
        };

        run_session(socket, active, state, repo, start_idx, resumed).await;
    })
}

async fn worker_ws_handler(
    ws:           WebSocketUpgrade,
    Path(branch): Path<String>,
    Query(query): Query<WorkerQuery>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let cfg  = read_config();
        let repo = effective_repo(&cfg);

        let (active, start_idx, resumed) = {
            let existing = state.worker_sessions.lock().unwrap().get(&branch).cloned();
            if let Some(sess) = existing {
                let idx = query.seq.unwrap_or(0);
                println!("[{}] /workers/{} reconnect (seq={})", sess.id, branch, idx);
                (sess, idx, true)
            } else if let Some((msgs, evs, cwd, sys)) = load_session_from_disk(&branch) {
                let active = new_active_session_with_data(branch.clone(), sys, cwd, msgs, evs);
                state.worker_sessions.lock().unwrap().insert(branch.clone(), active.clone());
                let idx = query.seq.unwrap_or(0);
                println!("[{}] /workers/{} restored from disk (seq={})", branch, branch, idx);
                (active, idx, true)
            } else {
                let system = build_system_prompt(&repo, Some(&branch), None);
                let active = new_active_session(branch.clone(), system.clone(), repo.clone());
                persist_meta(&branch, &repo, &system);
                state.worker_sessions.lock().unwrap().insert(branch.clone(), active.clone());
                println!("[{}] /workers/{} new session", branch, branch);
                (active, 0, false)
            }
        };

        let cwd = active.inner.lock().unwrap().cwd.clone();
        run_session(socket, active, state, cwd, start_idx, resumed).await;
    })
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    init_shell_env();

    let args: Vec<String> = std::env::args().collect();
    let key_file = std::env::var("NOISE_KEY_FILE")
        .unwrap_or_else(|_| NOISE_KEY_FILE.to_string());

    // --print-pubkey: emit base32 public key and exit (used by docker-entrypoint)
    if args.get(1).map(|s| s.as_str()) == Some("--print-pubkey") {
        let (_, public) = load_or_generate_keypair(&key_file);
        println!("{}", to_base32(&public));
        return;
    }

    let (static_private, static_public) = load_or_generate_keypair(&key_file);

    let noise_port: u16 = std::env::var("NOISE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9000);

    let http_port: u16 = 8000;

    println!("[claudulhu] Noise public key: {}", to_base32(&static_public));

    // Start Noise proxy on a separate task
    tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

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
        .route("/completions",     get(get_completions_handler))
        .route("/config",          get(get_config_handler).put(update_config_handler))
        .route("/chat",            get(chat_ws_handler))
        .route("/workers/:branch", get(worker_ws_handler))
        .with_state(state)
        .layer(cors);

    // HTTP/WebSocket server: localhost-only, proxied via Noise
    let addr = format!("127.0.0.1:{http_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await
        .expect("failed to bind HTTP port");
    println!("[claudulhu] HTTP/WebSocket on {addr} (Noise proxy on 0.0.0.0:{noise_port})");

    axum::serve(listener, app).await.unwrap();
}
