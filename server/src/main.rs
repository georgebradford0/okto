use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use tracing::{debug, error, info, warn};

use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use octo_core::{
    build_ephemeral_system_prompt, build_system_prompt, build_tools_with_mcp,
    chain_executor_with_mcp, effective_repo, get_branches_for_repo,
    init_mcp_pool, init_shell_env, load_or_generate_keypair, read_config, resolve_api_key,
    resolve_model, run_noise_proxy, send_message, to_base32, write_config, ApiMessage, AnthropicTool,
    ChatEvent, Config, ContentBlock, McpPool, DEV_PUBKEY_BASE32, DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC,
};
use octo_k8s_ops::k8s;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};

const NOISE_KEY_FILE: &str = "/etc/octo/noise_key.bin";

// ── Session persistence ───────────────────────────────────────────────────────

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("OCTO_DATA_DIR") {
        PathBuf::from(d)
    } else {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".octo")
    }
}

fn session_dir() -> PathBuf { data_dir().join("session") }

fn save_messages(messages: &[ApiMessage]) {
    let dir = session_dir();
    fs::create_dir_all(&dir).ok();
    if let Ok(json) = serde_json::to_string(messages) {
        let path = dir.join("messages.json");
        if let Err(e) = fs::write(&path, json) {
            error!("[server] failed to save messages to {}: {e}", path.display());
        } else {
            debug!("[server] saved {} message(s) to {}", messages.len(), path.display());
        }
    }
}

fn load_messages() -> Vec<ApiMessage> {
    let path = session_dir().join("messages.json");
    match fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()) {
        Some(msgs) => {
            let v: Vec<ApiMessage> = msgs;
            info!("[server] loaded {} message(s) from {}", v.len(), path.display());
            v
        }
        None => {
            debug!("[server] no saved messages at {}", path.display());
            vec![]
        }
    }
}

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct HistMsg {
    role: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
}

fn messages_to_history(messages: &[ApiMessage], last_cost_usd: Option<f64>) -> Vec<HistMsg> {
    // Build tool_use_id → output text from ToolResult blocks in user messages.
    let mut tool_outputs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in messages {
        if m.role == "user" {
            for block in &m.content {
                if let ContentBlock::ToolResult { tool_use_id, content } = block {
                    let text = content.first()
                        .and_then(|v| v["text"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    tool_outputs.insert(tool_use_id.clone(), text);
                }
            }
        }
    }

    let mut result = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "user" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                if !text.is_empty() { result.push(HistMsg { role: "user".to_string(), text, cost_usd: None, output: None }); }
            }
            "interrupted" => {
                result.push(HistMsg { role: "interrupted".to_string(), text: "interrupted".to_string(), cost_usd: None, output: None });
            }
            "error" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                result.push(HistMsg { role: "error".to_string(), text, cost_usd: None, output: None });
            }
            "assistant" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                if !text.is_empty() { result.push(HistMsg { role: "assistant".to_string(), text, cost_usd: None, output: None }); }
                for block in &m.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let preview = input.as_object()
                            .and_then(|map| map.values().next())
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim().to_string());
                        let text = match preview {
                            Some(p) => format!("{name}({p})"),
                            None    => name.clone(),
                        };
                        let output = tool_outputs.get(id).cloned();
                        result.push(HistMsg { role: "tool".to_string(), text, cost_usd: None, output });
                    }
                }
            }
            _ => {}
        }
    }
    // Attach cost to the last assistant message.
    if let Some(cost) = last_cost_usd {
        for msg in result.iter_mut().rev() {
            if msg.role == "assistant" {
                msg.cost_usd = Some(cost);
                break;
            }
        }
    }
    result
}

// ── App state ─────────────────────────────────────────────────────────────────

// Holds the live streaming state shared between the active streaming loop and any watchers.
// Events are buffered so that a watcher joining mid-turn can replay everything it missed.
struct StreamState {
    buffer: Vec<String>,
    subs:   Vec<mpsc::UnboundedSender<String>>,
}

struct AppState {
    messages:      Arc<Mutex<Vec<ApiMessage>>>,
    last_cost_usd: Mutex<Option<f64>>,
    system:        String,
    cwd:           String,
    /// Buffered events for the current turn + live subscriber list.
    stream_state:  Mutex<StreamState>,
    /// True while a /stream loop is running.
    is_streaming:  AtomicBool,
    /// Cancellation token for the current streaming turn. Replaced at the start of each turn.
    cancel:        Mutex<CancellationToken>,
    mcp_pool:      McpPool,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse { (StatusCode::OK, "ok") }

async fn interrupt_handler(State(state): State<Arc<AppState>>) -> StatusCode {
    state.cancel.lock().unwrap().cancel();
    StatusCode::OK
}

async fn history_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cost = *state.last_cost_usd.lock().unwrap();
    let msgs = messages_to_history(&state.messages.lock().unwrap(), cost);
    let is_streaming = state.is_streaming.load(Ordering::Relaxed);
    Json(serde_json::json!({ "messages": msgs, "is_streaming": is_streaming }))
}

#[derive(Deserialize)]
struct PostMessage { text: String }

async fn message_handler(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<PostMessage>,
) -> impl IntoResponse {
    let preview: String = body.text.chars().take(120).collect();
    info!("[server/message_handler] received ({} chars): {preview}", body.text.len());
    let start = Instant::now();

    let api_key = match resolve_api_key() {
        Some(k) => k,
        None    => {
            error!("[server/message_handler] no API key configured");
            return (StatusCode::INTERNAL_SERVER_ERROR,
                           Json(serde_json::json!({"error": "no API key configured"}))).into_response();
        }
    };
    let model = resolve_model();

    // Ephemeral loop — does not touch the shared conversation history.
    let messages = vec![ApiMessage {
        role:    "user".to_string(),
        content: vec![ContentBlock::Text { text: body.text }],
    }];

    info!("[server/message_handler] calling ephemeral send_message");
    let extra_tools = build_tools_with_mcp(&state.mcp_pool, &make_extra_tools()).await;
    let executor    = chain_executor_with_mcp(state.mcp_pool.clone(), make_extra_executor());
    match send_message(messages, build_ephemeral_system_prompt(), &model, &api_key, &state.cwd, None, CancellationToken::new(), &extra_tools, executor).await {
        Ok((text, cost_usd, _)) => {
            let elapsed = start.elapsed().as_millis();
            info!("[server/message_handler] done in {elapsed}ms cost=${cost_usd:.4} response=({} chars)", text.len());
            (StatusCode::OK, Json(serde_json::json!({ "text": text, "cost_usd": cost_usd }))).into_response()
        }
        Err((e, _)) => {
            let elapsed = start.elapsed().as_millis();
            error!("[server/message_handler] error in {elapsed}ms: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))).into_response()
        }
    }
}

async fn stream_handler(
    ws:           WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_stream(socket, state))
}

/// Render a `ChatEvent` to the JSON shape the wire schema (mobile/src/wire.ts)
/// expects. Returns `None` for variants that aren't part of the /stream protocol.
fn chat_event_to_wire_json(event: &ChatEvent) -> Option<serde_json::Value> {
    match event {
        ChatEvent::Text { text } =>
            Some(serde_json::json!({"type":"text","text":text})),
        ChatEvent::ToolUse { tool, input } =>
            Some(serde_json::json!({"type":"tool_use","tool":tool,"input":input})),
        ChatEvent::ToolOutput { line } =>
            Some(serde_json::json!({"type":"tool_output","line":line})),
        ChatEvent::ToolResult { tool_use_id, content } =>
            Some(serde_json::json!({"type":"tool_result","tool_use_id":tool_use_id,"output":content})),
        ChatEvent::Result { cost_usd, .. } =>
            Some(serde_json::json!({"type":"done","cost_usd":cost_usd})),
        ChatEvent::Interrupted { cost_usd } =>
            Some(serde_json::json!({"type":"interrupted","cost_usd":cost_usd})),
        ChatEvent::InterruptAck =>
            Some(serde_json::json!({"type":"interrupt_ack"})),
        ChatEvent::Error { message } =>
            Some(serde_json::json!({"type":"error","message":message})),
        ChatEvent::System { text } =>
            Some(serde_json::json!({"type":"system","text":text})),
        _ => None,
    }
}

/// Push a JSON-serialized event to the per-turn buffer and fan it out to every
/// live WS subscriber. Mirrors the previous in-line block but factored out so
/// the spawn_turn task can call it without holding any other state.
fn buffer_and_fanout(state: &AppState, json: String) {
    let mut ss = state.stream_state.lock().unwrap();
    ss.buffer.push(json.clone());
    ss.subs.retain(|tx| tx.send(json.clone()).is_ok());
}

/// Spawn an agentic turn. Returns immediately; events are buffered + fanned out
/// to all current /stream subscribers. The caller must have already verified
/// `is_streaming` was false and flipped it to true.
fn spawn_turn(state: Arc<AppState>, text: String) {
    tokio::spawn(async move {
        let api_key = match resolve_api_key() {
            Some(k) => k,
            None => {
                let errmsg = "no API key configured".to_string();
                {
                    let mut msgs = state.messages.lock().unwrap();
                    msgs.push(ApiMessage {
                        role:    "error".to_string(),
                        content: vec![ContentBlock::Text { text: errmsg.clone() }],
                    });
                    save_messages(&msgs);
                }
                let json = serde_json::json!({"type":"error","message": errmsg}).to_string();
                buffer_and_fanout(&state, json);
                state.is_streaming.store(false, Ordering::Relaxed);
                return;
            }
        };
        let model = resolve_model();

        {
            let mut msgs = state.messages.lock().unwrap();
            msgs.push(ApiMessage {
                role:    "user".to_string(),
                content: vec![ContentBlock::Text { text: text.clone() }],
            });
            save_messages(&msgs);
        }

        let messages: Vec<ApiMessage> = state.messages.lock().unwrap().iter()
            .filter(|m| m.role != "interrupted" && m.role != "error")
            .cloned()
            .collect();
        let system    = state.system.clone();
        let cwd       = state.cwd.clone();
        let msgs_arc  = state.messages.clone();
        let state_arc = Arc::clone(&state);

        let (event_tx, mut event_rx) = mpsc::channel::<ChatEvent>(256);
        let done_tx = event_tx.clone();

        // Fresh cancellation token for this turn; stored on AppState so /interrupt
        // and incoming "interrupt" frames can reach it.
        let cancel = CancellationToken::new();
        *state.cancel.lock().unwrap() = cancel.clone();

        // Clear the per-turn buffer; subscribers stay so live events still fan out.
        state.stream_state.lock().unwrap().buffer.clear();

        let extra_tools = build_tools_with_mcp(&state.mcp_pool, &make_extra_tools()).await;
        let executor    = chain_executor_with_mcp(state.mcp_pool.clone(), make_extra_executor());

        // Agent task: drives the model loop, terminates with Result/Interrupted/Error.
        tokio::spawn(async move {
            match send_message(messages, &system, &model, &api_key, &cwd, Some(event_tx), cancel.clone(), &extra_tools, executor).await {
                Ok((_, cost_usd, mut updated)) => {
                    if cancel.is_cancelled() {
                        updated.push(ApiMessage {
                            role:    "interrupted".to_string(),
                            content: vec![ContentBlock::Text { text: "interrupted".to_string() }],
                        });
                        *msgs_arc.lock().unwrap() = updated.clone();
                        save_messages(&updated);
                        *state_arc.last_cost_usd.lock().unwrap() = Some(cost_usd);
                        done_tx.send(ChatEvent::Interrupted { cost_usd }).await.ok();
                    } else {
                        *msgs_arc.lock().unwrap() = updated.clone();
                        save_messages(&updated);
                        *state_arc.last_cost_usd.lock().unwrap() = Some(cost_usd);
                        done_tx.send(ChatEvent::Result {
                            cost_usd, turns: 0, session_id: String::new(), result: None,
                        }).await.ok();
                    }
                }
                Err((e, mut partial)) => {
                    partial.push(ApiMessage {
                        role:    "error".to_string(),
                        content: vec![ContentBlock::Text { text: e.clone() }],
                    });
                    *msgs_arc.lock().unwrap() = partial.clone();
                    save_messages(&partial);
                    done_tx.send(ChatEvent::Error { message: e }).await.ok();
                }
            }
        });

        // Relay task: drains the per-turn mpsc, buffers and fans out to all WS subs.
        while let Some(event) = event_rx.recv().await {
            if let Some(json) = chat_event_to_wire_json(&event) {
                buffer_and_fanout(&state, json.to_string());
            }
        }
        state.is_streaming.store(false, Ordering::Relaxed);
        info!("[server/stream] turn complete, is_streaming=false");
    });
}

async fn handle_stream(socket: WebSocket, state: Arc<AppState>) {
    info!("[server/stream] WebSocket connection opened");
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Atomically snapshot the buffer (events from any in-flight turn) and
    // register as a subscriber so no events are lost in the gap.
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<String>();
    let (replay, resumed) = {
        let mut ss = state.stream_state.lock().unwrap();
        let replay = ss.buffer.clone();
        ss.subs.push(sub_tx);
        (replay, state.is_streaming.load(Ordering::Relaxed))
    };

    // Greet the client. `resumed` indicates whether they're joining an in-flight
    // turn; if so the buffer replay below catches them up to its current state.
    let ready = serde_json::json!({"type":"ready","session_id":"","resumed":resumed}).to_string();
    if ws_tx.send(WsMessage::Text(ready)).await.is_err() {
        return;
    }
    if !replay.is_empty() {
        info!("[server/stream] replaying {} buffered event(s) to new connection", replay.len());
        for event in replay {
            if ws_tx.send(WsMessage::Text(event)).await.is_err() { return; }
        }
    }

    loop {
        tokio::select! {
            // Outgoing: agentic-turn events fanned out from spawn_turn / buffer.
            msg = sub_rx.recv() => match msg {
                Some(json) => {
                    if ws_tx.send(WsMessage::Text(json)).await.is_err() { break; }
                }
                None => break,
            },

            // Incoming: client frames.
            msg = ws_rx.next() => match msg {
                Some(Ok(WsMessage::Text(t))) => {
                    handle_client_frame(&t, &state).await;
                }
                Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => continue,
                Some(Ok(WsMessage::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => continue,
            },
        }
    }

    info!("[server/stream] connection closed");
}

/// Dispatch a client → server frame parsed from a /stream WS message.
async fn handle_client_frame(raw: &str, state: &Arc<AppState>) {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v)  => v,
        Err(_) => {
            warn!("[server/stream] dropping unparseable client frame");
            return;
        }
    };
    let frame_type = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
    match frame_type {
        "user_message" => {
            let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if text.is_empty() {
                warn!("[server/stream] user_message frame missing/empty text");
                return;
            }
            if state.is_streaming
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                let json = serde_json::json!({"type":"error","message":"a turn is already running"}).to_string();
                buffer_and_fanout(state, json);
                return;
            }
            let preview: String = text.chars().take(120).collect();
            info!("[server/stream] user_message ({} chars): {preview}", text.len());
            spawn_turn(state.clone(), text);
        }
        "interrupt" => {
            info!("[server/stream] interrupt frame received");
            state.cancel.lock().unwrap().cancel();
            buffer_and_fanout(state, serde_json::json!({"type":"interrupt_ack"}).to_string());
        }
        "pong" => {
            // App-level keepalive ack — handled per-WS in the future ping/pong work.
        }
        other => {
            warn!("[server/stream] unknown client frame type='{other}'");
        }
    }
}

async fn clear_handler(State(state): State<Arc<AppState>>) -> StatusCode {
    info!("[server/clear] clearing conversation history");
    let mut msgs = state.messages.lock().unwrap();
    msgs.clear();
    save_messages(&msgs);
    StatusCode::OK
}

#[derive(Deserialize)]
struct CompletionQuery { dir_part: Option<String>, file_part: Option<String> }

async fn get_completions_handler(Query(p): Query<CompletionQuery>) -> Json<Vec<String>> {
    let repo      = effective_repo();
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
    let repo = effective_repo();
    match get_branches_for_repo(&repo) {
        Ok(b)  => Json(b).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_config_handler() -> Json<Config> { Json(read_config()) }

async fn update_config_handler(Json(patch): Json<Config>) -> StatusCode {
    info!(
        "[server/config] update model={:?} api_key={}",
        patch.model,
        if patch.api_key.is_some() { "provided" } else { "unchanged" }
    );
    let mut cfg = read_config();
    if patch.api_key.is_some() { cfg.api_key = patch.api_key; }
    if patch.model.is_some()   { cfg.model   = patch.model; }
    write_config(&cfg);
    StatusCode::OK
}

// ── Parent messaging tools ─────────────────────────────────────────────────────

fn message_lair_tool() -> AnthropicTool {
    AnthropicTool {
        name: "message_lair".to_string(),
        description: "Send a message to the parent (lair) container's agent and wait for its \
                       response. Use this to request secrets, configuration, or other information \
                       held by the parent. The parent will respond with a text reply."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The message to send to the parent agent."
                }
            },
            "required": ["text"]
        }),
    }
}

fn make_extra_tools() -> Vec<AnthropicTool> {
    // Only add the tool if LAIR_URL is configured.
    if std::env::var("LAIR_URL").is_ok() {
        vec![message_lair_tool()]
    } else {
        vec![]
    }
}

fn make_extra_executor() -> Option<Arc<dyn Fn(String, serde_json::Value)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
    + Send + Sync>>
{
    let lair_url = match std::env::var("LAIR_URL") {
        Ok(u) => u,
        Err(_) => return None,
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build message_lair HTTP client");
    Some(Arc::new(move |name: String, input: serde_json::Value| {
        let lair_url = lair_url.clone();
        let client = client.clone();
        Box::pin(async move {
            if name != "message_lair" {
                return format!("unknown tool: {name}");
            }
            let text = match input.get("text").and_then(|v| v.as_str()) {
                Some(t) => t.to_string(),
                None => return "error: missing 'text' field".to_string(),
            };
            let preview: String = text.chars().take(120).collect();
            let url = format!("{}/message", lair_url.trim_end_matches('/'));
            info!("[server/message_lair] → POST {url} ({} chars): {preview}", text.len());
            let start = Instant::now();
            match client
                .post(&url)
                .json(&serde_json::json!({ "text": text }))
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    let elapsed = start.elapsed().as_millis();
                    info!("[server/message_lair] ← HTTP {status} in {elapsed}ms");
                    match resp.json::<serde_json::Value>().await {
                        Ok(body) => {
                            let result = body
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("(no response text)")
                                .to_string();
                            let rpreview: String = result.chars().take(120).collect();
                            info!("[server/message_lair] response ({} chars): {rpreview}", result.len());
                            result
                        }
                        Err(e) => {
                            error!("[server/message_lair] parse error: {e}");
                            format!("error parsing parent response: {e}")
                        }
                    }
                }
                Err(e) => {
                    let elapsed = start.elapsed().as_millis();
                    error!("[server/message_lair] request failed in {elapsed}ms: {e}");
                    format!("error contacting parent: {e}")
                }
            }
        })
    }))
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    init_shell_env();

    let args: Vec<String> = std::env::args().collect();
    let is_dev   = std::env::var("OCTO_DEV").as_deref() == Ok("1");
    let key_file = std::env::var("NOISE_KEY_FILE").unwrap_or_else(|_| NOISE_KEY_FILE.to_string());

    let injected_keypair: Option<(Vec<u8>, Vec<u8>)> = std::env::var("NOISE_PRIVATE_KEY").ok()
        .and_then(|s| {
            let bytes = hex::decode(s.trim()).ok()?;
            if bytes.len() == 64 {
                Some((bytes[..32].to_vec(), bytes[32..].to_vec()))
            } else {
                None
            }
        });

    if args.get(1).map(|s| s.as_str()) == Some("--print-pubkey") {
        let pubkey = if is_dev {
            DEV_PUBKEY_BASE32.to_string()
        } else if let Some((_, public)) = &injected_keypair {
            to_base32(public)
        } else {
            let (_, public) = load_or_generate_keypair(&key_file);
            to_base32(&public)
        };
        println!("{pubkey}");
        return;
    }

    let (static_private, static_public) = if is_dev {
        warn!("[server] DEV MODE: using fixed dev keypair (OCTO_DEV=1)");
        (DEV_STATIC_PRIVATE.to_vec(), DEV_STATIC_PUBLIC.to_vec())
    } else if let Some(kp) = injected_keypair {
        kp
    } else {
        load_or_generate_keypair(&key_file)
    };

    let noise_port: u16 = std::env::var("NOISE_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(9000);
    let http_port:  u16 = 8000;
    let lair_url = std::env::var("LAIR_URL").unwrap_or_default();

    info!("[server] noise_pubkey={} noise_port={noise_port} http_port={http_port}", to_base32(&static_public));
    if lair_url.is_empty() {
        info!("[server] LAIR_URL not set — message_lair tool disabled");
    } else {
        info!("[server] LAIR_URL={lair_url} — message_lair tool enabled");
    }

    tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

    // Stamp our version onto the deployment annotation so `octo reload`
    // can display the version transition for child pods.
    if let Ok(deployment_name) = std::env::var("DEPLOYMENT_NAME") {
        tokio::spawn(async move {
            match k8s::build_client().await {
                Ok(client) => {
                    if let Err(e) = k8s::stamp_deployment_version(&client, &deployment_name, env!("CARGO_PKG_VERSION")).await {
                        warn!("[server] could not stamp version annotation: {e}");
                    } else {
                        info!("[server] stamped version {} on deployment/{deployment_name}", env!("CARGO_PKG_VERSION"));
                    }
                }
                Err(e) => warn!("[server] k8s client unavailable, skipping version stamp: {e}"),
            }
        });
    }

    let repo     = effective_repo();
    let system   = build_system_prompt(&repo);
    let messages = load_messages();
    info!("[server] loaded {} message(s) from history, repo={repo}", messages.len());

    let mcp_pool = init_mcp_pool().await;

    let state = Arc::new(AppState {
        messages:      Arc::new(Mutex::new(messages)),
        last_cost_usd: Mutex::new(None),
        system,
        cwd: repo.clone(),
        stream_state:  Mutex::new(StreamState { buffer: Vec::new(), subs: Vec::new() }),
        is_streaming:  AtomicBool::new(false),
        cancel:        Mutex::new(CancellationToken::new()),
        mcp_pool,
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::PUT, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let app = Router::new()
        .route("/health",      get(health_handler))
        .route("/history",     get(history_handler))
        .route("/message",     post(message_handler))
        .route("/stream",      get(stream_handler))
        .route("/interrupt",   post(interrupt_handler))
        .route("/clear",       post(clear_handler))
        .route("/branches",    get(get_branches_handler))
        .route("/completions", get(get_completions_handler))
        .route("/config",      get(get_config_handler).put(update_config_handler))
        .with_state(state.clone())
        .layer(cors);

    let addr = format!("0.0.0.0:{http_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind HTTP port");
    info!("[server] HTTP listening on {addr} (Noise proxy on 0.0.0.0:{noise_port}, repo: {repo})");

    if let Ok(prompt) = std::env::var("STARTUP_PROMPT") {
        if !prompt.is_empty() {
            let state_sp   = Arc::clone(&state);
            let api_key_sp = resolve_api_key().unwrap_or_default();
            let model_sp   = resolve_model();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                info!("[server] running STARTUP_PROMPT ({} chars)", prompt.len());
                state_sp.is_streaming.store(true, Ordering::Relaxed);
                {
                    let mut msgs = state_sp.messages.lock().unwrap();
                    msgs.push(ApiMessage {
                        role:    "user".to_string(),
                        content: vec![ContentBlock::Text { text: prompt.clone() }],
                    });
                    save_messages(&msgs);
                }
                let messages: Vec<ApiMessage> = state_sp.messages.lock().unwrap().iter()
                    .filter(|m| m.role != "interrupted" && m.role != "error")
                    .cloned()
                    .collect();
                let extra_tools = build_tools_with_mcp(&state_sp.mcp_pool, &make_extra_tools()).await;
                let executor    = chain_executor_with_mcp(state_sp.mcp_pool.clone(), make_extra_executor());
                match send_message(
                    messages,
                    &state_sp.system,
                    &model_sp,
                    &api_key_sp,
                    &state_sp.cwd,
                    None,
                    CancellationToken::new(),
                    &extra_tools,
                    executor,
                ).await {
                    Ok((_, cost_usd, updated)) => {
                        *state_sp.messages.lock().unwrap() = updated.clone();
                        save_messages(&updated);
                        *state_sp.last_cost_usd.lock().unwrap() = Some(cost_usd);
                        info!("[server] STARTUP_PROMPT complete cost=${cost_usd:.4}");
                    }
                    Err((e, mut partial)) => {
                        partial.push(ApiMessage {
                            role:    "error".to_string(),
                            content: vec![ContentBlock::Text { text: e.clone() }],
                        });
                        *state_sp.messages.lock().unwrap() = partial.clone();
                        save_messages(&partial);
                        error!("[server] STARTUP_PROMPT error: {e}");
                    }
                }
                state_sp.is_streaming.store(false, Ordering::Relaxed);
            });
        }
    }

    axum::serve(listener, app).await.unwrap();
}
