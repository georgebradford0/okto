use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axum::{
    extract::{
        ws::{Message, WebSocketUpgrade},
        State,
    },
    http::{Method, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use claudulhu_core::{
    init_mcp_pool, init_shell_env, load_or_generate_keypair, resolve_api_key, run_agentic_loop,
    run_noise_proxy, to_base32, ApiMessage, ChatEvent, ContentBlock, Session,
    DEV_PUBKEY_BASE32, DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify};
use tower_http::cors::{Any, CorsLayer};

// ── Noise Protocol ────────────────────────────────────────────────────────────

const NOISE_KEY_FILE: &str = "/data/noise_key.bin";

// ── Container registry ────────────────────────────────────────────────────────

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDULHU_DATA_DIR") {
        PathBuf::from(d)
    } else {
        PathBuf::from("/data")
    }
}

fn registry_path() -> PathBuf {
    data_dir().join("pubkey_registry.json")
}

fn load_pubkey_registry() -> HashMap<String, String> {
    fs::read_to_string(registry_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_pubkey_registry(registry: &HashMap<String, String>) {
    if let Ok(json) = serde_json::to_string(registry) {
        fs::write(registry_path(), json).ok();
    }
}

// ── Container types ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct ContainerInfo {
    id:      String,
    name:    String,
    git_url: String,
    status:  String,
    host:    String,
    port:    u16,
    pubkey:  String,
}

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsFrame {
    History      { messages: Vec<HistMsg>, live_gen: usize },
    Token        { text: String, live_gen: usize },
    Tool         { name: String, input: serde_json::Value, live_gen: usize },
    Question     { question: String, live_gen: usize },
    Done         { cost_usd: f64, live_gen: usize },
    Error        { message: String, live_gen: usize },
    SessionStart { label: String, session_id: String, live_gen: usize },
    SessionEnd   { summary: String, live_gen: usize },
    Ack          { live_gen: usize },
    // Master-specific frames
    ContainerList   { containers: Vec<ContainerInfo> },
    ContainerStatus { id: String, name: String, status: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct HistMsg {
    role: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Message   { text: String },
    Interrupt,
    Answer    { answer: String },
    Clear,
}

// ── Session persistence ───────────────────────────────────────────────────────

fn session_dir() -> PathBuf { data_dir().join("session") }

fn save_messages(messages: &[ApiMessage]) {
    let dir = session_dir();
    fs::create_dir_all(&dir).ok();
    if let Ok(json) = serde_json::to_string(messages) {
        fs::write(dir.join("messages.json"), json).ok();
    }
}

fn load_messages() -> Vec<ApiMessage> {
    fs::read_to_string(session_dir().join("messages.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn messages_to_history(messages: &[ApiMessage]) -> Vec<HistMsg> {
    messages.iter().filter_map(|m| {
        let text: String = m.content.iter()
            .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
            .collect();
        if text.is_empty() { None }
        else { Some(HistMsg { role: m.role.clone(), text }) }
    }).collect()
}

// ── Live event buffer ─────────────────────────────────────────────────────────

struct LiveState {
    buf:    Mutex<LiveBuffer>,
    notify: Notify,
}

#[derive(Default)]
struct LiveBuffer {
    gen:    usize,
    events: Vec<WsFrame>,
}

// ── App state ─────────────────────────────────────────────────────────────────

struct AppState {
    session:          Arc<Mutex<Session>>,
    loop_running:     Arc<AtomicBool>,
    live:             Arc<LiveState>,
    pubkey_b32:       String,
    containers:       Arc<Mutex<Vec<ContainerInfo>>>,
    container_notify: Arc<Notify>,
    public_host:      String,
}

// ── ChatEvent → WsFrame ───────────────────────────────────────────────────────

fn chat_event_to_frame(event: &ChatEvent, live_gen: usize) -> Option<WsFrame> {
    let v: serde_json::Value = serde_json::to_value(event).ok()?;
    match v["type"].as_str()? {
        "text"          => Some(WsFrame::Token        { text:     v["text"].as_str()?.to_string(), live_gen }),
        "tool_use"      => Some(WsFrame::Tool         { name: v["tool"].as_str()?.to_string(), input: v["input"].clone(), live_gen }),
        "result"        => Some(WsFrame::Done         { cost_usd: v["cost_usd"].as_f64().unwrap_or(0.0), live_gen }),
        "interrupted"   => Some(WsFrame::Done         { cost_usd: v["cost_usd"].as_f64().unwrap_or(0.0), live_gen }),
        "error"         => Some(WsFrame::Error        { message:  v["message"].as_str()?.to_string(), live_gen }),
        "question"      => Some(WsFrame::Question     { question: v["question"].as_str()?.to_string(), live_gen }),
        "session_start" => Some(WsFrame::SessionStart { label: v["label"].as_str()?.to_string(), session_id: v["session_id"].as_str()?.to_string(), live_gen }),
        "session_end"   => Some(WsFrame::SessionEnd   { summary: v["summary"].as_str()?.to_string(), live_gen }),
        _               => None,
    }
}

// ── Live delivery task ────────────────────────────────────────────────────────

async fn deliver_live(live: Arc<LiveState>, tx: mpsc::Sender<String>, start_gen: usize, start_idx: usize) {
    let mut gen = start_gen;
    let mut idx = start_idx;
    loop {
        loop {
            let frame = {
                let buf = live.buf.lock().unwrap();
                if buf.gen != gen { gen = buf.gen; idx = 0; }
                buf.events.get(idx).cloned()
            };
            match frame {
                Some(f) => {
                    if tx.send(serde_json::to_string(&f).unwrap_or_default()).await.is_err() { return; }
                    idx += 1;
                }
                None => break,
            }
        }
        live.notify.notified().await;
    }
}

// ── Container update delivery task ────────────────────────────────────────────
//
// Waits for the container_notify signal and pushes the full ContainerList to
// a single connected client.  One of these is spawned per WebSocket connection.

async fn deliver_container_updates(
    containers:       Arc<Mutex<Vec<ContainerInfo>>>,
    container_notify: Arc<Notify>,
    tx:               mpsc::Sender<String>,
) {
    loop {
        container_notify.notified().await;
        let list = containers.lock().unwrap().clone();
        let frame = serde_json::to_string(&WsFrame::ContainerList { containers: list })
            .unwrap_or_default();
        if tx.send(frame).await.is_err() { break; }
    }
}

// ── WebSocket handler ─────────────────────────────────────────────────────────

async fn chat_ws_handler(
    ws:           WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let (mut ws_sink, mut ws_stream) = socket.split();
        let (ws_tx, mut ws_rx) = mpsc::channel::<String>(256);

        tokio::spawn(async move {
            while let Some(json) = ws_rx.recv().await {
                if ws_sink.send(Message::Text(json)).await.is_err() { break; }
            }
        });

        // Snapshot history and live state consistently.
        let (history_json, start_gen, start_idx) = {
            let buf = state.live.buf.lock().unwrap();
            let loop_running = state.loop_running.load(Ordering::SeqCst);
            let live_gen = buf.gen;
            let (start_gen, start_idx) = if loop_running {
                (buf.gen, 0usize)
            } else {
                (buf.gen, buf.events.len())
            };
            let mut hist_msgs = messages_to_history(&state.session.lock().unwrap().messages);
            if loop_running {
                if let Some(last) = hist_msgs.last() {
                    if last.role == "assistant" { hist_msgs.pop(); }
                }
            }
            let history = WsFrame::History { messages: hist_msgs, live_gen };
            (serde_json::to_string(&history).unwrap_or_default(), start_gen, start_idx)
        };
        ws_tx.send(history_json).await.ok();

        // Send current container list immediately on connect.
        let containers_json = {
            let list = state.containers.lock().unwrap().clone();
            serde_json::to_string(&WsFrame::ContainerList { containers: list }).unwrap_or_default()
        };
        ws_tx.send(containers_json).await.ok();

        // Deliver live chat events.
        let deliver = tokio::spawn(deliver_live(state.live.clone(), ws_tx.clone(), start_gen, start_idx));

        // Deliver container updates for the lifetime of this connection.
        let deliver_cont = tokio::spawn(deliver_container_updates(
            state.containers.clone(),
            state.container_notify.clone(),
            ws_tx.clone(),
        ));

        // Receive messages from client.
        while let Some(Ok(msg)) = ws_stream.next().await {
            let text = match msg {
                Message::Text(t)  => t,
                Message::Close(_) => break,
                _                 => continue,
            };
            let client_msg: ClientMsg = match serde_json::from_str(&text) {
                Ok(m)  => m,
                Err(_) => continue,
            };

            match client_msg {
                ClientMsg::Message { text } => {
                    let api_key = match resolve_api_key() {
                        Some(k) => k,
                        None    => {
                            let live_gen = state.live.buf.lock().unwrap().gen;
                            ws_tx.send(serde_json::to_string(&WsFrame::Error {
                                message: "no API key configured".into(),
                                live_gen,
                            }).unwrap_or_default()).await.ok();
                            continue;
                        }
                    };
                    let cfg   = claudulhu_core::read_config();
                    let model = cfg.model.unwrap_or_else(|| "claude-sonnet-4-6".to_string());

                    {
                        let mut s = state.session.lock().unwrap();
                        s.aborted.store(false, Ordering::Relaxed);
                        s.messages.push(ApiMessage {
                            role:    "user".to_string(),
                            content: vec![ContentBlock::Text { text }],
                        });
                        save_messages(&s.messages);
                    }

                    let ack_gen;
                    if !state.loop_running.swap(true, Ordering::SeqCst) {
                        let new_gen = {
                            let mut buf = state.live.buf.lock().unwrap();
                            buf.gen += 1;
                            buf.events.clear();
                            buf.gen
                        };
                        ack_gen = new_gen;
                        ws_tx.send(serde_json::to_string(&WsFrame::Ack { live_gen: ack_gen }).unwrap_or_default()).await.ok();
                        state.live.notify.notify_waiters();

                        let (loop_tx, mut loop_rx) = mpsc::channel::<ChatEvent>(256);
                        let session_c    = state.session.clone();
                        let live_c       = state.live.clone();
                        let loop_running = state.loop_running.clone();

                        tokio::spawn(async move {
                            run_agentic_loop(session_c.clone(), "main".to_string(), api_key, model, loop_tx).await;
                            loop_running.store(false, Ordering::SeqCst);
                            save_messages(&session_c.lock().unwrap().messages);
                        });

                        tokio::spawn(async move {
                            while let Some(event) = loop_rx.recv().await {
                                if let Some(frame) = chat_event_to_frame(&event, new_gen) {
                                    live_c.buf.lock().unwrap().events.push(frame);
                                    live_c.notify.notify_waiters();
                                }
                            }
                        });
                    } else {
                        ack_gen = state.live.buf.lock().unwrap().gen;
                        eprintln!("[chat] warning: message received while loop already running");
                        ws_tx.send(serde_json::to_string(&WsFrame::Ack { live_gen: ack_gen }).unwrap_or_default()).await.ok();
                    }
                }

                ClientMsg::Interrupt => {
                    state.session.lock().unwrap().aborted.store(true, Ordering::Relaxed);
                }

                ClientMsg::Answer { answer } => {
                    let pq   = state.session.lock().unwrap().pending_question.clone();
                    let mut slot = pq.lock().await;
                    if let Some(sender) = slot.take() { sender.send(answer).ok(); }
                }

                ClientMsg::Clear => {
                    {
                        let mut s = state.session.lock().unwrap();
                        s.messages.clear();
                        save_messages(&s.messages);
                    }
                    let live_gen = {
                        let mut buf = state.live.buf.lock().unwrap();
                        buf.gen += 1;
                        buf.events.clear();
                        buf.gen
                    };
                    state.live.notify.notify_waiters();
                    let json = serde_json::to_string(&WsFrame::History { messages: vec![], live_gen })
                        .unwrap_or_default();
                    ws_tx.send(json).await.ok();
                }
            }
        }

        deliver.abort();
        deliver_cont.abort();
        println!("[chat] WebSocket disconnected");
    })
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse { (StatusCode::OK, "ok") }

async fn info_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "pubkey": state.pubkey_b32 }))
}

// ── Container poller ──────────────────────────────────────────────────────────

async fn poll_containers(state: Arc<AppState>) {
    // Brief startup delay so Docker is ready.
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        match fetch_managed_containers(&state.public_host).await {
            Ok(mut new_containers) => {
                let mut registry = load_pubkey_registry();
                let mut dirty    = false;

                for c in &mut new_containers {
                    if let Some(pk) = registry.get(&c.id) {
                        c.pubkey = pk.clone();
                    } else if c.status == "running" {
                        println!("[containers] fetching pubkey for {}", c.name);
                        if let Some(pk) = fetch_pubkey_via_exec(&c.name).await {
                            c.pubkey = pk.clone();
                            registry.insert(c.id.clone(), pk);
                            dirty = true;
                        } else {
                            eprintln!("[containers] pubkey fetch failed for {}", c.name);
                        }
                    }
                }

                if dirty { save_pubkey_registry(&registry); }

                let changed = {
                    let current = state.containers.lock().unwrap();
                    *current != new_containers
                };
                if changed {
                    *state.containers.lock().unwrap() = new_containers;
                    state.container_notify.notify_waiters();
                    println!("[containers] state changed — notified clients");
                }
            }
            Err(e) => eprintln!("[containers] poll error: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn fetch_managed_containers(public_host: &str) -> anyhow::Result<Vec<ContainerInfo>> {
    // Get short IDs of all managed containers (running or stopped).
    let ids_out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("docker")
            .args(["ps", "-a", "--filter", "label=claudulhu.managed=1", "-q"])
            .output(),
    ).await.map_err(|_| anyhow::anyhow!("docker ps timed out"))?
    .map_err(|e| anyhow::anyhow!("docker ps failed: {e}"))?;

    let ids: Vec<&str> = std::str::from_utf8(&ids_out.stdout)?
        .lines()
        .filter(|l| !l.is_empty())
        .collect();

    if ids.is_empty() { return Ok(vec![]); }

    // Inspect all at once for full metadata.
    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("inspect");
    for id in &ids { cmd.arg(id); }
    let inspect_out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await.map_err(|_| anyhow::anyhow!("docker inspect timed out"))?
        .map_err(|e| anyhow::anyhow!("docker inspect failed: {e}"))?;

    let inspect: Vec<serde_json::Value> = serde_json::from_slice(&inspect_out.stdout)?;
    let mut results = Vec::new();

    for c in inspect {
        let id   = c["Id"].as_str().unwrap_or("").chars().take(12).collect::<String>();
        let name = c["Name"].as_str().unwrap_or("").trim_start_matches('/').to_string();
        let status = c["State"]["Status"].as_str().unwrap_or("unknown").to_string();

        let noise_port: u16 = c["Config"]["Env"]
            .as_array()
            .and_then(|env| {
                env.iter().find_map(|e| {
                    e.as_str()?.strip_prefix("NOISE_PORT=").and_then(|v| v.parse().ok())
                })
            })
            .unwrap_or(9100);

        let git_url = c["Config"]["Labels"]["claudulhu.git_url"]
            .as_str().unwrap_or("").to_string();

        results.push(ContainerInfo {
            id,
            name,
            git_url,
            status,
            host: public_host.to_string(),
            port: noise_port,
            pubkey: String::new(), // filled in by poll_containers
        });
    }

    Ok(results)
}

/// Run `docker exec <name> claudulhu-server --print-pubkey` to get a child's
/// Noise public key without any HTTP round-trip.
async fn fetch_pubkey_via_exec(container_name: &str) -> Option<String> {
    let fut = tokio::process::Command::new("docker")
        .args(["exec", container_name, "claudulhu-server", "--print-pubkey"])
        .output();
    let out = tokio::time::timeout(Duration::from_secs(5), fut).await.ok()?.ok()?;
    if !out.status.success() { return None; }
    let pk = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if pk.is_empty() { None } else { Some(pk) }
}

// ── System prompt ─────────────────────────────────────────────────────────────

const MASTER_SYSTEM_PROMPT: &str = "\
You are the master control node for a fleet of claudulhu coding assistant containers. \
You have full bash access with the Docker socket available.\n\n\
Standard child image: ghcr.io/georgebradford0/claudulhu-server:latest\n\n\
When creating child containers use:\n\
  --network claudulhu-net\n\
  --label claudulhu.managed=1\n\
  --label claudulhu.git_url=<url>\n\
  NOISE_PORT set to a free port in CHILD_PORT_RANGE (default 9100-9199)\n\
  Named volumes for /data and /workspace\n\
  Required env vars: ANTHROPIC_API_KEY, GIT_URL, GH_TOKEN\n\
  IMPORTANT: Always check that $GH_TOKEN is set before creating a child container.\n\
  If it is not set, do not create the container — tell the user GH_TOKEN is required.\n\
  When it is set, always pass these env vars to every child container:\n\
    -e ANTHROPIC_API_KEY=$ANTHROPIC_API_KEY\n\
    -e GH_TOKEN=$GH_TOKEN\n\
    -e PUBLIC_HOST=$PUBLIC_HOST\n\n\
Use bash freely: docker ps, docker start/stop/rm, docker logs, docker inspect, \
and any other system commands.\n\n\
Do not narrate or comment while working. Perform all tool calls silently. \
After all work is complete, provide one short summary of what was done and the outcome.";

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    init_shell_env();

    let args: Vec<String> = std::env::args().collect();
    let is_dev   = std::env::var("CLAUDULHU_DEV").as_deref() == Ok("1");
    let key_file = std::env::var("NOISE_KEY_FILE").unwrap_or_else(|_| NOISE_KEY_FILE.to_string());

    if args.get(1).map(|s| s.as_str()) == Some("--print-pubkey") {
        let pubkey = if is_dev {
            DEV_PUBKEY_BASE32.to_string()
        } else {
            let (_, public) = load_or_generate_keypair(&key_file);
            to_base32(&public)
        };
        println!("{pubkey}");
        return;
    }

    let (static_private, static_public) = if is_dev {
        println!("[claudulhu-rulyeh] !! DEV MODE: using fixed dev keypair");
        (DEV_STATIC_PRIVATE.to_vec(), DEV_STATIC_PUBLIC.to_vec())
    } else {
        load_or_generate_keypair(&key_file)
    };

    let pubkey_b32  = to_base32(&static_public);
    let noise_port: u16 = std::env::var("NOISE_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(9000);
    let http_port:  u16 = 8000;
    let public_host = std::env::var("PUBLIC_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());

    println!("[claudulhu-rulyeh] Noise public key: {pubkey_b32}");

    tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

    // Initialise data directory and load persisted session.
    let dir = data_dir();
    fs::create_dir_all(&dir).ok();
    let messages = load_messages();
    println!("[claudulhu-rulyeh] loaded {} message(s) from history", messages.len());

    let mcp_pool = init_mcp_pool().await;

    let containers       = Arc::new(Mutex::new(Vec::<ContainerInfo>::new()));
    let container_notify = Arc::new(Notify::new());

    let state = Arc::new(AppState {
        session: Arc::new(Mutex::new(Session {
            messages,
            system_prompt: MASTER_SYSTEM_PROMPT.to_string(),
            cwd:           "/".to_string(),
            aborted:          Arc::new(AtomicBool::new(false)),
            pending_question: Arc::new(tokio::sync::Mutex::new(None)),
            mcp_pool,
        })),
        loop_running:     Arc::new(AtomicBool::new(false)),
        live:             Arc::new(LiveState {
            buf:    Mutex::new(LiveBuffer::default()),
            notify: Notify::new(),
        }),
        pubkey_b32,
        containers:       containers.clone(),
        container_notify: container_notify.clone(),
        public_host:      public_host.clone(),
    });

    // Background container poller.
    tokio::spawn(poll_containers(state.clone()));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::OPTIONS])
        .allow_headers(Any);

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/info",   get(info_handler))
        .route("/chat",   get(chat_ws_handler))
        .with_state(state)
        .layer(cors);

    let addr = format!("127.0.0.1:{http_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind HTTP port");
    println!("[claudulhu-rulyeh] HTTP/WebSocket on {addr} (Noise proxy on 0.0.0.0:{noise_port})");

    axum::serve(listener, app).await.unwrap();
}
