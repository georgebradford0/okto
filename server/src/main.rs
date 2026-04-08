use std::{
    fs,
    io::Write as _,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Query, State},
    http::{Method, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use claudulhu_core::{
    build_system_prompt, effective_repo, get_branches_for_repo, init_mcp_pool, init_shell_env,
    read_config, resolve_api_key, run_agentic_loop, write_config, ApiMessage, ChatEvent, Config,
    ContentBlock, Session,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, Notify},
};
use tower_http::cors::{Any, CorsLayer};

// ── Noise Protocol ────────────────────────────────────────────────────────────

const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
const NOISE_KEY_FILE: &str = "/etc/claudulhu/noise_key.bin";

/// Fixed dev keypair — always the same so the mobile app can hardcode the public key.
/// Active when CLAUDULHU_DEV=1.  Generated once; DO NOT rotate.
const DEV_STATIC_PRIVATE: [u8; 32] = [
    0x6a, 0x58, 0xeb, 0x21, 0x90, 0x00, 0xf0, 0x5f,
    0xd2, 0x6a, 0xf1, 0x58, 0x74, 0xc6, 0x69, 0xbd,
    0x76, 0x01, 0xf8, 0x18, 0x27, 0x11, 0x66, 0xc7,
    0xa2, 0xb1, 0x3e, 0x54, 0x8b, 0xa5, 0x48, 0xbc,
];
const DEV_STATIC_PUBLIC: [u8; 32] = [
    0xdf, 0x3b, 0xff, 0xd5, 0xd2, 0xcc, 0x47, 0x19,
    0xd0, 0x3f, 0xbe, 0x27, 0x3f, 0x16, 0x5e, 0xd6,
    0x39, 0x0c, 0x62, 0xab, 0x82, 0x44, 0x77, 0xf2,
    0xed, 0x1c, 0x01, 0xaf, 0xfb, 0x60, 0xa7, 0x71,
];
/// Base32(DEV_STATIC_PUBLIC) — matches the hardcoded pk in mobile/App.tsx
const DEV_PUBKEY_BASE32: &str = "34577VOSZRDRTUB7XYTT6FS62Y4QYYVLQJCHP4XNDQA2763AU5YQ";

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

async fn read_noise_frame(stream: &mut tokio::net::TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn write_noise_frame(stream: &mut tokio::net::TcpStream, data: &[u8]) -> anyhow::Result<()> {
    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(data).await?;
    Ok(())
}

async fn noise_handshake(
    stream: &mut tokio::net::TcpStream,
    static_private: &[u8],
) -> anyhow::Result<snow::TransportState> {
    let builder = snow::Builder::new(NOISE_PATTERN.parse()?);
    let mut hs = builder.local_private_key(static_private).build_responder()?;
    let mut payload = vec![0u8; 65535];
    let msg1 = read_noise_frame(stream).await?;
    hs.read_message(&msg1, &mut payload)?;
    let mut msg2 = vec![0u8; 65535];
    let n = hs.write_message(&[], &mut msg2)?;
    write_noise_frame(stream, &msg2[..n]).await?;
    let msg3 = read_noise_frame(stream).await?;
    hs.read_message(&msg3, &mut payload)?;
    Ok(hs.into_transport_mode()?)
}

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
    let task_a = tokio::spawn(async move {
        let mut plain = vec![0u8; 65000];
        let mut enc   = vec![0u8; 65535];
        loop {
            let n = local_read.read(&mut plain).await.unwrap_or(0);
            if n == 0 { break; }
            let enc_n = match transport_enc.lock().unwrap().write_message(&plain[..n], &mut enc) {
                Ok(n)  => n,
                Err(_) => break,
            };
            let len = (enc_n as u16).to_be_bytes();
            if raw_write.write_all(&len).await.is_err()         { break; }
            if raw_write.write_all(&enc[..enc_n]).await.is_err() { break; }
        }
    });
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
                Ok(n)  => n,
                Err(_) => break,
            };
            if local_write.write_all(&dec[..dec_n]).await.is_err() { break; }
        }
    });
    tokio::select! { _ = task_a => {} _ = task_b => {} }
    Ok(())
}

async fn run_noise_proxy(static_private: Vec<u8>, noise_port: u16, http_port: u16) {
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{noise_port}"))
        .await.expect("failed to bind Noise port");
    println!("[noise] listening on 0.0.0.0:{noise_port} → 127.0.0.1:{http_port}");
    let static_private = Arc::new(static_private);
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        println!("[noise] connection from {peer}");
        let priv_clone = static_private.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_noise_connection(stream, priv_clone, http_port).await {
                eprintln!("[noise] error from {peer}: {e}");
            }
        });
    }
}

// ── Wire types ────────────────────────────────────────────────────────────────

/// Frames sent from server to mobile client over WebSocket.
#[derive(serde::Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsFrame {
    /// Full message history, sent once on connect.
    /// `live_gen` tells the client which generation of live events to expect next,
    /// so it can discard stale replays from a previous connection.
    History  { messages: Vec<HistMsg>, live_gen: usize },
    /// Streaming text token from the current assistant response.
    Token    { text: String, live_gen: usize },
    /// Tool being invoked (display only).  `input` was added in 0.0.19.
    Tool     { name: String, input: serde_json::Value, live_gen: usize },
    /// Claude is asking the user a question and needs an answer.
    Question { question: String, live_gen: usize },
    /// Current response is complete.
    Done { cost_usd: f64, live_gen: usize },
    /// Current response ended with an error.
    Error    { message: String, live_gen: usize },
    /// Model is beginning a multi-step agentic session.
    SessionStart { label: String, session_id: String, live_gen: usize },
    /// Model is ending an agentic session; summary is the final prose response.
    SessionEnd   { summary: String, live_gen: usize },
    /// Acknowledgement that the user message was saved server-side.
    /// `live_gen` is the generation the client should expect for the upcoming
    /// live frames so it doesn't discard them as stale.
    Ack { live_gen: usize },
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct HistMsg {
    role: String,
    text: String,
}

/// Frames sent from mobile client to server over WebSocket.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Message   { text: String },
    Interrupt,
    Answer    { answer: String },
    Clear,
}

// ── Session persistence ───────────────────────────────────────────────────────

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDULHU_DATA_DIR") {
        PathBuf::from(d)
    } else {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claudulhu")
    }
}

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

/// Convert internal API messages to the wire history format (text-only, skipping empty turns).
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
//
// Accumulates WsFrames for the current in-progress response.
// Cleared (with generation bump) at the start of each new response.
// Allows reconnecting clients to replay the full current response from the top.

struct LiveState {
    buf:    Mutex<LiveBuffer>,
    notify: Notify,
}

#[derive(Default)]
struct LiveBuffer {
    /// Incremented each time a new response starts (live events cleared).
    gen:    usize,
    events: Vec<WsFrame>,
}

// ── App state ─────────────────────────────────────────────────────────────────

struct AppState {
    session:      Arc<Mutex<Session>>,
    loop_running: Arc<AtomicBool>,
    live:         Arc<LiveState>,
}

// ── ChatEvent → WsFrame ───────────────────────────────────────────────────────

fn chat_event_to_frame(event: &ChatEvent, live_gen: usize) -> Option<WsFrame> {
    // Go through JSON so we're not coupled to internal enum layout.
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
//
// Streams current live events to a WebSocket writer channel.
// - start_gen / start_idx: atomically snapshotted by the connect handler so
//   that history and the live-replay starting point are always consistent.
// - On gen change: reset to idx=0 so every new response is fully delivered.

async fn deliver_live(live: Arc<LiveState>, tx: mpsc::Sender<String>, start_gen: usize, start_idx: usize) {
    let mut gen = start_gen;
    let mut idx = start_idx;

    loop {
        loop {
            let frame = {
                let buf = live.buf.lock().unwrap();
                if buf.gen != gen {
                    gen = buf.gen;
                    idx = 0;
                }
                buf.events.get(idx).cloned()
            };
            match frame {
                Some(f) => {
                    if tx.send(serde_json::to_string(&f).unwrap_or_default()).await.is_err() {
                        return;
                    }
                    idx += 1;
                }
                None => break,
            }
        }
        live.notify.notified().await;
    }
}

// ── WebSocket handler ─────────────────────────────────────────────────────────

const MOBILE_SYSTEM_PROMPT_SUFFIX: &str = "\n\n\
You are being accessed from a mobile client where screen space is limited. \
Do not narrate, explain, or comment while you work. \
Perform all tool calls silently. \
Only after all work is complete, provide a single short summary of what was done and the outcome.";

async fn chat_ws_handler(
    ws:           WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let (mut ws_sink, mut ws_stream) = socket.split();
        let (ws_tx, mut ws_rx) = mpsc::channel::<String>(256);

        // Writer task: drain ws_tx → WebSocket.
        tokio::spawn(async move {
            while let Some(json) = ws_rx.recv().await {
                if ws_sink.send(Message::Text(json)).await.is_err() { break; }
            }
        });

        // Snapshot (loop_running, live_gen, live_idx, history) consistently so
        // that the history frame and the live-replay starting point never
        // double-deliver the last assistant turn.
        //
        // The tricky race: the agentic loop pushes the completed assistant message
        // into session.messages *before* clearing loop_running.  If we read
        // loop_running=true but session already contains the finished assistant
        // turn, we would send it in the history frame AND replay it via live
        // tokens → duplicate text on screen.
        //
        // Fix: hold the live.buf lock while reading both loop_running and
        // session.messages.  The loop pushes to session without holding live.buf,
        // but more importantly: when loop_running is true we strip any trailing
        // assistant message from the history snapshot — the live token replay
        // will reconstruct it from idx=0.
        let (history_json, start_gen, start_idx) = {
            let buf = state.live.buf.lock().unwrap();
            let loop_running = state.loop_running.load(Ordering::SeqCst);
            let live_gen = buf.gen;
            let (start_gen, start_idx) = if loop_running {
                // Loop is still running — replay live events from the beginning
                // of the current generation so the client sees every token.
                (buf.gen, 0usize)
            } else {
                // Loop is idle — history already contains the completed text.
                // Start past the end of the current gen so we only deliver
                // future responses (next gen will reset idx to 0 automatically).
                (buf.gen, buf.events.len())
            };
            // Read session while live.buf is still locked to prevent the loop
            // from committing a new assistant message between the two reads.
            let mut hist_msgs = messages_to_history(&state.session.lock().unwrap().messages);
            // If the loop is running, the live token replay covers the in-progress
            // (or just-completed) assistant turn.  Strip any trailing assistant
            // message from the history snapshot to avoid duplication.
            if loop_running {
                if let Some(last) = hist_msgs.last() {
                    if last.role == "assistant" {
                        hist_msgs.pop();
                    }
                }
            }
            let history = WsFrame::History { messages: hist_msgs, live_gen };
            (serde_json::to_string(&history).unwrap_or_default(), start_gen, start_idx)
        };
        ws_tx.send(history_json).await.ok();

        // Deliver live events for the current (or future) response.
        let deliver = tokio::spawn(deliver_live(state.live.clone(), ws_tx.clone(), start_gen, start_idx));

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
                    let cfg     = read_config();
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

                    // Only start the loop if it isn't already running.  Bump
                    // buf.gen BEFORE sending Ack so the client knows which
                    // live_gen to expect for the upcoming frames.
                    let ack_gen;
                    if !state.loop_running.swap(true, Ordering::SeqCst) {
                        // Reset live buffer for the new response.
                        let new_gen = {
                            let mut buf = state.live.buf.lock().unwrap();
                            buf.gen += 1;
                            buf.events.clear();
                            buf.gen
                        };
                        ack_gen = new_gen;

                        // Acknowledge after gen bump so the Ack carries the correct live_gen.
                        ws_tx.send(serde_json::to_string(&WsFrame::Ack { live_gen: ack_gen }).unwrap_or_default()).await.ok();
                        state.live.notify.notify_waiters();

                        let (loop_tx, mut loop_rx) = mpsc::channel::<ChatEvent>(256);
                        let session_c    = state.session.clone();
                        let live_c       = state.live.clone();
                        let loop_running = state.loop_running.clone();

                        // Run the agentic loop; clear the running flag then save messages.
                        // Order matters: set loop_running=false FIRST so any client
                        // that connects after the loop ends sees idle state and does not
                        // replay live events (which would duplicate the saved history).
                        tokio::spawn(async move {
                            run_agentic_loop(
                                session_c.clone(), "main".to_string(), api_key, model, loop_tx,
                            ).await;
                            loop_running.store(false, Ordering::SeqCst);
                            save_messages(&session_c.lock().unwrap().messages);
                        });

                        // Forward ChatEvents from the loop to the live buffer.
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
                    // Clear session messages first, then reset live buffer.
                    // Take each lock separately (not nested) to avoid inversion.
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
        println!("[chat] WebSocket disconnected");
    })
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse { (StatusCode::OK, "ok") }

#[derive(Deserialize)]
struct CompletionQuery { dir_part: Option<String>, file_part: Option<String> }

async fn get_completions_handler(Query(p): Query<CompletionQuery>) -> Json<Vec<String>> {
    let cfg       = read_config();
    let repo      = effective_repo(&cfg);
    let dir_part  = p.dir_part.unwrap_or_default();
    let file_part = p.file_part.unwrap_or_default();
    let mut seen    = std::collections::HashSet::new();
    let mut results = Vec::new();
    let search_dir  = PathBuf::from(&repo).join(&dir_part);
    if let Ok(entries) = fs::read_dir(&search_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && !file_part.starts_with('.') { continue; }
            if !name.to_lowercase().starts_with(&file_part.to_lowercase()) { continue; }
            let is_dir     = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let completion = if is_dir { format!("{dir_part}{name}/") } else { format!("{dir_part}{name}") };
            if seen.insert(completion.clone()) { results.push(completion); }
        }
    }
    results.sort();
    Json(results)
}

async fn get_branches_handler() -> impl IntoResponse {
    let cfg  = read_config();
    let repo = effective_repo(&cfg);
    match get_branches_for_repo(&repo) {
        Ok(b)  => Json(b).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_config_handler() -> Json<Config> { Json(read_config()) }

async fn update_config_handler(Json(patch): Json<Config>) -> StatusCode {
    let mut cfg = read_config();
    if patch.repo.is_some()    { cfg.repo    = patch.repo; }
    if patch.api_key.is_some() { cfg.api_key = patch.api_key; }
    if patch.model.is_some()   { cfg.model   = patch.model; }
    write_config(&cfg);
    StatusCode::OK
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    init_shell_env();

    let args: Vec<String> = std::env::args().collect();
    let is_dev = std::env::var("CLAUDULHU_DEV").as_deref() == Ok("1");

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
        println!("[claudulhu] !! DEV MODE: using fixed dev keypair (CLAUDULHU_DEV=1)");
        (DEV_STATIC_PRIVATE.to_vec(), DEV_STATIC_PUBLIC.to_vec())
    } else {
        load_or_generate_keypair(&key_file)
    };

    let noise_port: u16 = std::env::var("NOISE_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(9000);
    let http_port:  u16 = 8000;
    println!("[claudulhu] Noise public key: {}", to_base32(&static_public));

    tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

    // Build session from current config + persisted messages.
    let cfg  = read_config();
    let repo = effective_repo(&cfg);
    let mut system = build_system_prompt(&repo, None, None);
    system.push_str(MOBILE_SYSTEM_PROMPT_SUFFIX);
    let messages = load_messages();
    println!("[claudulhu] loaded {} message(s) from history", messages.len());

    let mcp_pool = init_mcp_pool().await;

    let state = Arc::new(AppState {
        session: Arc::new(Mutex::new(Session {
            messages,
            system_prompt: system,
            cwd:           repo.clone(),
            aborted:          Arc::new(AtomicBool::new(false)),
            pending_question: Arc::new(tokio::sync::Mutex::new(None)),
            mcp_pool,
        })),
        loop_running: Arc::new(AtomicBool::new(false)),
        live: Arc::new(LiveState {
            buf:    Mutex::new(LiveBuffer::default()),
            notify: Notify::new(),
        }),
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::PUT, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let app = Router::new()
        .route("/health",      get(health_handler))
        .route("/branches",    get(get_branches_handler))
        .route("/completions", get(get_completions_handler))
        .route("/config",      get(get_config_handler).put(update_config_handler))
        .route("/chat",        get(chat_ws_handler))
        .with_state(state)
        .layer(cors);

    let addr = format!("127.0.0.1:{http_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind HTTP port");
    println!("[claudulhu] HTTP/WebSocket on {addr} (Noise proxy on 0.0.0.0:{noise_port}, repo: {repo})");

    axum::serve(listener, app).await.unwrap();
}
