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
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
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
    hs.read_message(&msg1, &mut payload)?;

    // Message 2: → e, ee, s, es
    let mut msg2 = vec![0u8; 65535];
    let n = hs.write_message(&[], &mut msg2)?;
    write_noise_frame(stream, &msg2[..n]).await?;

    // Message 3: ← s, se
    let msg3 = read_noise_frame(stream).await?;
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

// ── App State ─────────────────────────────────────────────────────────────────

struct AppState {
    sessions:        Mutex<HashMap<String, Arc<Mutex<Session>>>>,
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
