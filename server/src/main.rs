use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use tracing::{error, info, warn};

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
use claudulhu_core::{
    build_ephemeral_system_prompt, build_system_prompt, effective_repo, get_branches_for_repo,
    init_shell_env, load_or_generate_keypair, read_config, resolve_api_key, run_noise_proxy,
    send_message, to_base32, write_config, ApiMessage, AnthropicTool, ChatEvent, Config,
    ContentBlock, DEV_PUBKEY_BASE32, DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};

const NOISE_KEY_FILE: &str = "/etc/claudulhu/noise_key.bin";

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

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct HistMsg {
    role: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
}

fn messages_to_history(messages: &[ApiMessage], last_cost_usd: Option<f64>) -> Vec<HistMsg> {
    let mut result = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "user" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                if !text.is_empty() { result.push(HistMsg { role: "user".to_string(), text, cost_usd: None }); }
            }
            "interrupted" => {
                result.push(HistMsg { role: "interrupted".to_string(), text: "interrupted".to_string(), cost_usd: None });
            }
            "assistant" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                if !text.is_empty() { result.push(HistMsg { role: "assistant".to_string(), text, cost_usd: None }); }
                for block in &m.content {
                    if let ContentBlock::ToolUse { name, input, .. } = block {
                        let preview = input.as_object()
                            .and_then(|map| map.values().next())
                            .and_then(|v| v.as_str())
                            .map(|s| {
                                let s = s.trim();
                                // safe truncation at char boundary
                                let limit = 60;
                                if s.len() <= limit { s.to_string() }
                                else {
                                    let b = s.char_indices().take_while(|(i, _)| *i <= limit).last().map(|(i, _)| i).unwrap_or(limit);
                                    format!("{}…", &s[..b])
                                }
                            });
                        let text = match preview {
                            Some(p) => format!("{name}({p})"),
                            None    => name.clone(),
                        };
                        result.push(HistMsg { role: "tool".to_string(), text, cost_usd: None });
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

struct AppState {
    messages:      Arc<Mutex<Vec<ApiMessage>>>,
    last_cost_usd: Mutex<Option<f64>>,
    system:        String,
    cwd:           String,
    /// Broadcast channel for live stream events — watchers subscribe here.
    stream_tx:     broadcast::Sender<String>,
    /// True while a /stream loop is running.
    is_streaming:  AtomicBool,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse { (StatusCode::OK, "ok") }

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
    let model = read_config().model.unwrap_or_else(|| "claude-sonnet-4-6".to_string());

    // Ephemeral loop — does not touch the shared conversation history.
    let messages = vec![ApiMessage {
        role:    "user".to_string(),
        content: vec![ContentBlock::Text { text: body.text }],
    }];

    info!("[server/message_handler] calling ephemeral send_message");
    match send_message(messages, build_ephemeral_system_prompt(), &model, &api_key, &state.cwd, None, Arc::new(AtomicBool::new(false)), &make_extra_tools(), make_extra_executor()).await {
        Ok((text, cost_usd, _)) => {
            let elapsed = start.elapsed().as_millis();
            info!("[server/message_handler] done in {elapsed}ms cost=${cost_usd:.4} response=({} chars)", text.len());
            (StatusCode::OK, Json(serde_json::json!({ "text": text, "cost_usd": cost_usd }))).into_response()
        }
        Err(e) => {
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

async fn handle_stream(socket: WebSocket, state: Arc<AppState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Read first frame: either {"text":"..."} to start a new loop,
    // or {"type":"watch"} to attach to an already-running loop.
    let first = loop {
        match ws_rx.next().await {
            Some(Ok(WsMessage::Text(t))) => {
                match serde_json::from_str::<serde_json::Value>(&t).ok() {
                    Some(v) => break v,
                    None    => return,
                }
            }
            Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => continue,
            _ => return,
        }
    };

    // ── Watch mode: just subscribe to the broadcast and relay ─────────────────
    if first.get("type").and_then(|v| v.as_str()) == Some("watch") {
        let mut rx = state.stream_tx.subscribe();
        loop {
            match rx.recv().await {
                Ok(json_str) => {
                    if ws_tx.send(WsMessage::Text(json_str)).await.is_err() { break; }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
        return;
    }

    // ── New loop ──────────────────────────────────────────────────────────────
    let text = match first.get("text").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None    => return,
    };

    let api_key = match resolve_api_key() {
        Some(k) => k,
        None => {
            let msg = serde_json::json!({"type":"error","message":"no API key configured"}).to_string();
            ws_tx.send(WsMessage::Text(msg)).await.ok();
            return;
        }
    };
    let model = read_config().model.unwrap_or_else(|| "claude-sonnet-4-6".to_string());

    {
        let mut msgs = state.messages.lock().unwrap();
        msgs.push(ApiMessage {
            role:    "user".to_string(),
            content: vec![ContentBlock::Text { text: text.clone() }],
        });
        save_messages(&msgs);
    }

    let messages: Vec<ApiMessage> = state.messages.lock().unwrap().iter()
        .filter(|m| m.role != "interrupted")
        .cloned()
        .collect();
    let system   = state.system.clone();
    let cwd      = state.cwd.clone();
    let msgs_arc  = state.messages.clone();
    let state_arc = Arc::clone(&state);

    let (event_tx, mut event_rx) = mpsc::channel::<ChatEvent>(256);
    let done_tx = event_tx.clone();

    let aborted             = Arc::new(AtomicBool::new(false));
    let aborted_for_listener = aborted.clone();

    state.is_streaming.store(true, Ordering::Relaxed);

    tokio::spawn(async move {
        while let Some(Ok(WsMessage::Text(t))) = ws_rx.next().await {
            if serde_json::from_str::<serde_json::Value>(&t)
                .ok()
                .and_then(|v| v["type"].as_str().map(str::to_string))
                .as_deref() == Some("interrupt")
            {
                aborted_for_listener.store(true, Ordering::Relaxed);
                break;
            }
        }
    });

    tokio::spawn(async move {
        match send_message(messages, &system, &model, &api_key, &cwd, Some(event_tx), aborted.clone(), &make_extra_tools(), make_extra_executor()).await {
            Ok((_, cost_usd, mut updated)) => {
                if aborted.load(Ordering::Relaxed) {
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
            Err(e) => {
                msgs_arc.lock().unwrap().pop();
                save_messages(&msgs_arc.lock().unwrap());
                done_tx.send(ChatEvent::Error { message: e }).await.ok();
            }
        }
    });

    while let Some(event) = event_rx.recv().await {
        let json_opt: Option<serde_json::Value> = match event {
            ChatEvent::Text { text } =>
                Some(serde_json::json!({"type":"text","text":text})),
            ChatEvent::ToolUse { tool, input } =>
                Some(serde_json::json!({"type":"tool_use","tool":tool,"input":input})),
            ChatEvent::Result { cost_usd, .. } =>
                Some(serde_json::json!({"type":"done","cost_usd":cost_usd})),
            ChatEvent::Interrupted { cost_usd } =>
                Some(serde_json::json!({"type":"interrupted","cost_usd":cost_usd})),
            ChatEvent::Error { message } =>
                Some(serde_json::json!({"type":"error","message":message})),
            _ => None,
        };
        if let Some(json) = json_opt {
            let json_str = json.to_string();
            // Broadcast to any watchers (ignore send errors — no watchers is fine).
            state.stream_tx.send(json_str.clone()).ok();
            if ws_tx.send(WsMessage::Text(json_str)).await.is_err() { break; }
        }
    }
    state.is_streaming.store(false, Ordering::Relaxed);
}

async fn clear_handler(State(state): State<Arc<AppState>>) -> StatusCode {
    let mut msgs = state.messages.lock().unwrap();
    msgs.clear();
    save_messages(&msgs);
    StatusCode::OK
}

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

// ── Parent messaging tools ─────────────────────────────────────────────────────

fn message_parent_tool() -> AnthropicTool {
    AnthropicTool {
        name: "message_parent".to_string(),
        description: "Send a message to the parent (rulyeh) container's agent and wait for its \
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
    // Only add the tool if RULYEH_URL is configured.
    if std::env::var("RULYEH_URL").is_ok() {
        vec![message_parent_tool()]
    } else {
        vec![]
    }
}

fn make_extra_executor() -> Option<Arc<dyn Fn(String, serde_json::Value)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
    + Send + Sync>>
{
    let rulyeh_url = match std::env::var("RULYEH_URL") {
        Ok(u) => u,
        Err(_) => return None,
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build message_parent HTTP client");
    Some(Arc::new(move |name: String, input: serde_json::Value| {
        let rulyeh_url = rulyeh_url.clone();
        let client = client.clone();
        Box::pin(async move {
            if name != "message_parent" {
                return format!("unknown tool: {name}");
            }
            let text = match input.get("text").and_then(|v| v.as_str()) {
                Some(t) => t.to_string(),
                None => return "error: missing 'text' field".to_string(),
            };
            let preview: String = text.chars().take(120).collect();
            let url = format!("{}/message", rulyeh_url.trim_end_matches('/'));
            info!("[server/message_parent] → POST {url} ({} chars): {preview}", text.len());
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
                    info!("[server/message_parent] ← HTTP {status} in {elapsed}ms");
                    match resp.json::<serde_json::Value>().await {
                        Ok(body) => {
                            let result = body
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("(no response text)")
                                .to_string();
                            let rpreview: String = result.chars().take(120).collect();
                            info!("[server/message_parent] response ({} chars): {rpreview}", result.len());
                            result
                        }
                        Err(e) => {
                            error!("[server/message_parent] parse error: {e}");
                            format!("error parsing parent response: {e}")
                        }
                    }
                }
                Err(e) => {
                    let elapsed = start.elapsed().as_millis();
                    error!("[server/message_parent] request failed in {elapsed}ms: {e}");
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
        warn!("[server] DEV MODE: using fixed dev keypair (CLAUDULHU_DEV=1)");
        (DEV_STATIC_PRIVATE.to_vec(), DEV_STATIC_PUBLIC.to_vec())
    } else {
        load_or_generate_keypair(&key_file)
    };

    let noise_port: u16 = std::env::var("NOISE_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(9000);
    let http_port:  u16 = 8000;
    let rulyeh_url = std::env::var("RULYEH_URL").unwrap_or_default();

    info!("[server] noise_pubkey={} noise_port={noise_port} http_port={http_port}", to_base32(&static_public));
    if rulyeh_url.is_empty() {
        info!("[server] RULYEH_URL not set — message_parent tool disabled");
    } else {
        info!("[server] RULYEH_URL={rulyeh_url} — message_parent tool enabled");
    }

    tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

    let cfg      = read_config();
    let repo     = effective_repo(&cfg);
    let system   = build_system_prompt(&repo, None, None);
    let messages = load_messages();
    info!("[server] loaded {} message(s) from history, repo={repo}", messages.len());

    let (stream_tx, _) = broadcast::channel(256);
    let state = Arc::new(AppState {
        messages:      Arc::new(Mutex::new(messages)),
        last_cost_usd: Mutex::new(None),
        system,
        cwd: repo.clone(),
        stream_tx,
        is_streaming:  AtomicBool::new(false),
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
        .route("/clear",       post(clear_handler))
        .route("/branches",    get(get_branches_handler))
        .route("/completions", get(get_completions_handler))
        .route("/config",      get(get_config_handler).put(update_config_handler))
        .with_state(state)
        .layer(cors);

    let addr = format!("0.0.0.0:{http_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind HTTP port");
    info!("[server] HTTP listening on {addr} (Noise proxy on 0.0.0.0:{noise_port}, repo: {repo})");

    axum::serve(listener, app).await.unwrap();
}
