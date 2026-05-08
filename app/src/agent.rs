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
use octo_core::{
    self,
    build_agent_system_prompt, build_ephemeral_system_prompt, build_system_prompt,
    build_tools_with_mcp, chain_executor_with_mcp, completion_chat_event, data_dir,
    get_branches_for_repo, init_mcp_pool, init_shell_env, load_or_generate_keypair, read_config,
    resolve_api_key, resolve_model, run_background_task_tool, run_noise_proxy, send_message,
    spawn_background_task, to_base32, write_config, ApiMessage, AnthropicTool,
    BackgroundTaskParams, ChatEvent, Config, ContentBlock, McpPool, DEV_PUBKEY_BASE32,
    DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC, KEEPALIVE_INTERVAL, KEEPALIVE_MAX_MISSED,
    StreamState, buffer_and_fanout, chat_event_to_wire_json, messages_to_history,
    parse_ping_id, parse_pong_id,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use serde::Deserialize;
use tower_http::cors::{Any, CorsLayer};

const NOISE_KEY_FILE: &str = "/etc/octo/noise_key.bin";

// ── Session persistence ───────────────────────────────────────────────────────
//
// Thin local wrappers that bind the shared `octo_core::app` helpers to this
// binary's data dir and log prefix.

fn save_messages(messages: &[ApiMessage]) {
    octo_core::save_messages(&data_dir(), messages, "agent");
}

fn load_messages() -> Vec<ApiMessage> {
    octo_core::load_messages(&data_dir(), "agent")
}

// ── App state ─────────────────────────────────────────────────────────────────

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
    // is_streaming is no longer in the response — the persistent /stream WS
    // sends `ready { resumed }` on connect, which is what mobile uses to
    // decide whether to enter the 'streaming' UI state.
    Json(serde_json::json!({ "messages": msgs }))
}

#[derive(Deserialize)]
struct PostMessage { text: String }

async fn message_handler(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<PostMessage>,
) -> impl IntoResponse {
    let preview: String = body.text.chars().take(120).collect();
    info!("[agent/message_handler] received ({} chars): {preview}", body.text.len());
    let start = Instant::now();

    let api_key = match resolve_api_key() {
        Some(k) => k,
        None    => {
            error!("[agent/message_handler] no API key configured");
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

    info!("[agent/message_handler] calling ephemeral send_message");
    let extra_tools = build_tools_with_mcp(&state.mcp_pool, &make_extra_tools()).await;
    let executor    = chain_executor_with_mcp(state.mcp_pool.clone(), make_extra_executor(state.clone()));
    match send_message(messages, build_ephemeral_system_prompt(), &model, &api_key, &state.cwd, None, CancellationToken::new(), &extra_tools, executor).await {
        Ok((text, cost_usd, _)) => {
            let elapsed = start.elapsed().as_millis();
            info!("[agent/message_handler] done in {elapsed}ms cost=${cost_usd:.4} response=({} chars)", text.len());
            (StatusCode::OK, Json(serde_json::json!({ "text": text, "cost_usd": cost_usd }))).into_response()
        }
        Err((e, _)) => {
            let elapsed = start.elapsed().as_millis();
            error!("[agent/message_handler] error in {elapsed}ms: {e}");
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
                buffer_and_fanout(&state.stream_state, json);
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
        let executor    = chain_executor_with_mcp(state.mcp_pool.clone(), make_extra_executor(state.clone()));

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
                buffer_and_fanout(&state.stream_state, json.to_string());
            }
        }
        state.is_streaming.store(false, Ordering::Relaxed);
        // Drop the per-turn buffer so a between-turns reconnect doesn't replay
        // the just-finished turn on top of /history (would duplicate the last
        // assistant message client-side).
        state.stream_state.lock().unwrap().buffer.clear();
        info!("[agent/stream] turn complete, is_streaming=false");
    });
}

async fn handle_stream(socket: WebSocket, state: Arc<AppState>) {
    info!("[agent/stream] WebSocket connection opened");
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Atomically snapshot the buffer (events from any in-flight turn) and
    // register as a subscriber so no events are lost in the gap. The buffer
    // is only forwarded if a turn is genuinely in flight — between turns the
    // canonical state lives in /history, and replaying the just-finished
    // turn's events would duplicate the last assistant message client-side.
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<String>();
    let (replay, resumed) = {
        let mut ss = state.stream_state.lock().unwrap();
        ss.subs.push(sub_tx);
        let resumed = state.is_streaming.load(Ordering::Relaxed);
        let replay = if resumed { ss.buffer.clone() } else { Vec::new() };
        (replay, resumed)
    };

    // Greet the client. `resumed` indicates whether they're joining an in-flight
    // turn; if so the buffer replay below catches them up to its current state.
    let ready = serde_json::json!({"type":"ready","session_id":"","resumed":resumed}).to_string();
    if ws_tx.send(WsMessage::Text(ready)).await.is_err() {
        return;
    }
    if !replay.is_empty() {
        info!("[agent/stream] replaying {} buffered event(s) to new connection", replay.len());
        for event in replay {
            if ws_tx.send(WsMessage::Text(event)).await.is_err() { return; }
        }
    }

    // App-level keepalive (see lair/handle_stream for design).
    let mut ping_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + KEEPALIVE_INTERVAL,
        KEEPALIVE_INTERVAL,
    );
    let mut next_ping_id:  u64 = 0;
    let mut last_acked_id: u64 = 0;

    loop {
        tokio::select! {
            // Outgoing: agentic-turn events fanned out from spawn_turn / buffer.
            msg = sub_rx.recv() => match msg {
                Some(json) => {
                    if ws_tx.send(WsMessage::Text(json)).await.is_err() { break; }
                }
                None => break,
            },

            // Outgoing: keepalive ping.
            _ = ping_interval.tick() => {
                let outstanding = next_ping_id.saturating_sub(last_acked_id);
                if outstanding >= KEEPALIVE_MAX_MISSED {
                    warn!("[agent/stream] evicting peer: {outstanding} unacked ping(s)");
                    break;
                }
                next_ping_id += 1;
                let json = serde_json::json!({"type":"ping","id":next_ping_id}).to_string();
                if ws_tx.send(WsMessage::Text(json)).await.is_err() { break; }
            },

            // Incoming: client frames.
            msg = ws_rx.next() => match msg {
                Some(Ok(WsMessage::Text(t))) => {
                    if let Some(id) = parse_pong_id(&t) {
                        if id > last_acked_id { last_acked_id = id; }
                    } else if let Some(id) = parse_ping_id(&t) {
                        // Mobile-side keepalive — echo a pong on this same WS.
                        let json = serde_json::json!({"type":"pong","id":id}).to_string();
                        if ws_tx.send(WsMessage::Text(json)).await.is_err() { break; }
                    } else {
                        handle_client_frame(&t, &state).await;
                    }
                }
                Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => continue,
                Some(Ok(WsMessage::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => continue,
            },
        }
    }


    info!("[agent/stream] connection closed");
}

/// Dispatch a client → server frame parsed from a /stream WS message.
async fn handle_client_frame(raw: &str, state: &Arc<AppState>) {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v)  => v,
        Err(_) => {
            warn!("[agent/stream] dropping unparseable client frame");
            return;
        }
    };
    let frame_type = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
    match frame_type {
        "user_message" => {
            let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if text.is_empty() {
                warn!("[agent/stream] user_message frame missing/empty text");
                return;
            }
            if state.is_streaming
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                let json = serde_json::json!({"type":"error","message":"a turn is already running"}).to_string();
                buffer_and_fanout(&state.stream_state, json);
                return;
            }
            let preview: String = text.chars().take(120).collect();
            info!("[agent/stream] user_message ({} chars): {preview}", text.len());
            spawn_turn(state.clone(), text);
        }
        "interrupt" => {
            info!("[agent/stream] interrupt frame received");
            state.cancel.lock().unwrap().cancel();
            buffer_and_fanout(&state.stream_state, serde_json::json!({"type":"interrupt_ack"}).to_string());
        }
        "pong" => {
            // App-level keepalive ack — handled per-WS in the future ping/pong work.
        }
        other => {
            warn!("[agent/stream] unknown client frame type='{other}'");
        }
    }
}

async fn clear_handler(State(state): State<Arc<AppState>>) -> StatusCode {
    info!("[agent/clear] clearing conversation history");
    let mut msgs = state.messages.lock().unwrap();
    msgs.clear();
    save_messages(&msgs);
    StatusCode::OK
}

#[derive(Deserialize)]
struct CompletionQuery { dir_part: Option<String>, file_part: Option<String> }

async fn get_completions_handler(
    State(state): State<Arc<AppState>>,
    Query(p):     Query<CompletionQuery>,
) -> Json<Vec<String>> {
    let dir_part  = p.dir_part.unwrap_or_default();
    let file_part = p.file_part.unwrap_or_default();
    let mut seen    = std::collections::HashSet::new();
    let mut results = Vec::new();
    let search_dir  = PathBuf::from(&state.cwd).join(&dir_part);
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

async fn get_branches_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // No-repo agents have no branches — return [] rather than 500. The mobile
    // UI hides the branch picker in that case.
    if !PathBuf::from(&state.cwd).join(".git").is_dir() {
        return Json(Vec::<octo_core::Branch>::new()).into_response();
    }
    match get_branches_for_repo(&state.cwd) {
        Ok(b)  => Json(b).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_config_handler() -> Json<Config> { Json(read_config()) }

async fn update_config_handler(Json(patch): Json<Config>) -> StatusCode {
    info!(
        "[agent/config] update model={:?} api_key={}",
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
    let mut tools = vec![run_background_task_tool()];
    if std::env::var("LAIR_URL").is_ok() {
        tools.push(message_lair_tool());
    }
    tools
}

fn make_extra_executor(state: Arc<AppState>) -> Option<Arc<dyn Fn(String, serde_json::Value)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
    + Send + Sync>>
{
    let lair_url = std::env::var("LAIR_URL").ok();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build agent extra-tools HTTP client");
    Some(Arc::new(move |name: String, input: serde_json::Value| {
        let lair_url = lair_url.clone();
        let client = client.clone();
        let state  = state.clone();
        Box::pin(async move {
            match name.as_str() {
                "run_background_task" => exec_run_background_task(state, input).await,
                "message_lair" => {
                    let lair_url = match lair_url {
                        Some(u) => u,
                        None => return "error: message_lair not configured (LAIR_URL unset)".to_string(),
                    };
                    let text = match input.get("text").and_then(|v| v.as_str()) {
                        Some(t) => t.to_string(),
                        None => return "error: missing 'text' field".to_string(),
                    };
                    let preview: String = text.chars().take(120).collect();
                    let url = format!("{}/message", lair_url.trim_end_matches('/'));
                    info!("[agent/message_lair] → POST {url} ({} chars): {preview}", text.len());
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
                            info!("[agent/message_lair] ← HTTP {status} in {elapsed}ms");
                            match resp.json::<serde_json::Value>().await {
                                Ok(body) => {
                                    let result = body
                                        .get("text")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("(no response text)")
                                        .to_string();
                                    let rpreview: String = result.chars().take(120).collect();
                                    info!("[agent/message_lair] response ({} chars): {rpreview}", result.len());
                                    result
                                }
                                Err(e) => {
                                    error!("[agent/message_lair] parse error: {e}");
                                    format!("error parsing parent response: {e}")
                                }
                            }
                        }
                        Err(e) => {
                            let elapsed = start.elapsed().as_millis();
                            error!("[agent/message_lair] request failed in {elapsed}ms: {e}");
                            format!("error contacting parent: {e}")
                        }
                    }
                }
                other => format!("unknown tool: {other}"),
            }
        })
    }))
}

async fn exec_run_background_task(state: Arc<AppState>, input: serde_json::Value) -> String {
    let task_description = match input.get("task_description").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return "error: missing or empty 'task_description'".to_string(),
    };

    let api_key = match resolve_api_key() {
        Some(k) => k,
        None    => return "error: no API key configured for background task".to_string(),
    };
    let model = resolve_model();

    let task_id = format!("bg-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    info!("[agent/run_background_task] spawning {task_id} ({} chars)", task_description.len());

    let extra_tools = build_tools_with_mcp(&state.mcp_pool, &make_extra_tools()).await;
    let executor    = chain_executor_with_mcp(state.mcp_pool.clone(), make_extra_executor(state.clone()));

    let params = BackgroundTaskParams {
        task_id:          task_id.clone(),
        task_description,
        system:           state.system.clone(),
        model,
        api_key,
        cwd:              state.cwd.clone(),
        extra_tools,
        extra_executor:   executor,
    };

    let stream_state_arc = state.clone();
    spawn_background_task(params, move |outcome| {
        let event = completion_chat_event(&outcome);
        if let Some(json) = chat_event_to_wire_json(&event) {
            buffer_and_fanout(&stream_state_arc.stream_state, json.to_string());
        }
    });

    format!("Background task {task_id} started. The user will be notified when it completes.")
}

// ── Entry ─────────────────────────────────────────────────────────────────────

pub async fn run(print_pubkey: bool) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    init_shell_env();

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

    if print_pubkey {
        let pubkey = if is_dev {
            DEV_PUBKEY_BASE32.to_string()
        } else if let Some((_, public)) = &injected_keypair {
            to_base32(public)
        } else {
            let (_, public) = load_or_generate_keypair(&key_file);
            to_base32(&public)
        };
        println!("{pubkey}");
        return Ok(());
    }

    let (static_private, static_public) = if is_dev {
        warn!("[agent] DEV MODE: using fixed dev keypair (OCTO_DEV=1)");
        (DEV_STATIC_PRIVATE.to_vec(), DEV_STATIC_PUBLIC.to_vec())
    } else if let Some(kp) = injected_keypair {
        kp
    } else {
        load_or_generate_keypair(&key_file)
    };

    let noise_port:  u16 = std::env::var("NOISE_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(9000);
    let public_port: u16 = std::env::var("PUBLIC_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(noise_port);
    let http_port:   u16 = 8000;
    let lair_url    = std::env::var("LAIR_URL").unwrap_or_default();
    let public_host = crate::bootstrap::resolve_public_host("agent").await?;
    let pubkey_b32  = to_base32(&static_public);

    info!("[agent] noise_pubkey={pubkey_b32} noise_port={noise_port} http_port={http_port}");
    if lair_url.is_empty() {
        info!("[agent] LAIR_URL not set — message_lair tool disabled");
    } else {
        info!("[agent] LAIR_URL={lair_url} — message_lair tool enabled");
    }

    // Workspace + optional git repo. With GIT_URL set, behaviour matches the
    // old shell entrypoint exactly (clone-or-fetch, set git user, install a
    // credential helper). Without it, the workspace is just `mkdir -p` and
    // the agent runs there as a generic agent — see `build_agent_system_prompt`.
    let workspace = std::path::PathBuf::from(
        std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/workspace".to_string())
    );
    let git_url  = std::env::var("GIT_URL").ok();
    let gh_token = std::env::var("GH_TOKEN").ok();
    let has_repo = crate::bootstrap::ensure_workspace(
        &workspace,
        git_url.as_deref(),
        gh_token.as_deref(),
    ).await?;

    // STARTUP_SCRIPT runs after the workspace is populated so it can reference
    // the cloned repo (matches the bash entrypoint's order).
    crate::bootstrap::run_startup_script("agent").await?;

    tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

    // Children no longer have k8s API access, so stamping the deployment's
    // `octo.image-version` annotation is delegated to lair: we POST our name
    // and compiled version to lair's `/child-version` endpoint and lair patches
    // the annotation. Best-effort — log on failure but don't fail boot.
    if let (Ok(deployment_name), Ok(lair_url_for_stamp)) = (
        std::env::var("DEPLOYMENT_NAME"),
        std::env::var("LAIR_URL"),
    ) {
        tokio::spawn(async move {
            let url = format!("{}/child-version", lair_url_for_stamp.trim_end_matches('/'));
            let body = serde_json::json!({
                "name":    deployment_name,
                "version": env!("CARGO_PKG_VERSION"),
            });
            match reqwest::Client::new().post(&url).json(&body).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!("[agent] reported version {} to lair (deployment/{deployment_name})", env!("CARGO_PKG_VERSION"));
                }
                Ok(resp) => warn!("[agent] lair rejected version report: {}", resp.status()),
                Err(e)   => warn!("[agent] could not report version to lair: {e}"),
            }
        });
    }

    let cwd       = workspace.to_string_lossy().to_string();
    let system    = if has_repo {
        build_system_prompt(&cwd)
    } else {
        build_agent_system_prompt(&cwd)
    };
    let messages  = load_messages();
    info!(
        "[agent] loaded {} message(s) from history, cwd={cwd} (repo={})",
        messages.len(),
        if has_repo { "yes" } else { "no" },
    );

    let mcp_pool = init_mcp_pool().await;

    let state = Arc::new(AppState {
        messages:      Arc::new(Mutex::new(messages)),
        last_cost_usd: Mutex::new(None),
        system,
        cwd,
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
    let listener = tokio::net::TcpListener::bind(&addr).await
        .map_err(|e| anyhow::anyhow!("failed to bind HTTP port {addr}: {e}"))?;
    info!("[agent] HTTP listening on {addr} (Noise proxy on 0.0.0.0:{noise_port}, cwd: {})", state.cwd);

    // Listener is bound; the Noise port is reachable. Print the QR now so the
    // user never scans before the server can accept the connection.
    crate::bootstrap::print_qr("agent", &public_host, public_port, &pubkey_b32);

    if let Ok(prompt) = std::env::var("STARTUP_PROMPT") {
        if !prompt.is_empty() {
            let state_sp   = Arc::clone(&state);
            let api_key_sp = resolve_api_key().unwrap_or_default();
            let model_sp   = resolve_model();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                info!("[agent] running STARTUP_PROMPT ({} chars)", prompt.len());
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
                let executor    = chain_executor_with_mcp(state_sp.mcp_pool.clone(), make_extra_executor(state_sp.clone()));
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
                        info!("[agent] STARTUP_PROMPT complete cost=${cost_usd:.4}");
                    }
                    Err((e, mut partial)) => {
                        partial.push(ApiMessage {
                            role:    "error".to_string(),
                            content: vec![ContentBlock::Text { text: e.clone() }],
                        });
                        *state_sp.messages.lock().unwrap() = partial.clone();
                        save_messages(&partial);
                        error!("[agent] STARTUP_PROMPT error: {e}");
                    }
                }
                state_sp.is_streaming.store(false, Ordering::Relaxed);
            });
        }
    }

    axum::serve(listener, app).await
        .map_err(|e| anyhow::anyhow!("axum serve error: {e}"))?;
    Ok(())
}
