//! Child agent role. Runs an HTTP+WS server bound to `127.0.0.1:<AGENT_PORT>`.
//! Mobile never connects directly — lair proxies WebSocket traffic from its
//! own Noise tunnel into this server on demand.

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
    },
    time::Duration,
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
use okto_core::{
    self,
    build_agent_system_prompt, build_system_prompt,
    build_tools_with_mcp, cancel_task as core_cancel_task, chain_executor_with_mcp,
    completion_chat_event, data_dir, finalize_task, from_base32, now_secs,
    monitor_complete_message, monitor_complete_text,
    monitor_process_tool, monitor_progress_message, monitor_progress_text,
    register_task, tasks_wire_json, TaskOutput, TaskRecord, TaskStatus,
    DEFAULT_WAKE_INTERVAL_SECS, MIN_WAKE_INTERVAL_SECS,
    get_branches_for_repo, init_mcp_pool, init_shell_env,
    load_or_generate_keypair, read_config,
    resolve_api_key, resolve_model, run_command_in_background_tool,
    run_noise_proxy, send_message, send_notification_tool, NOTIFY_CATEGORY_AGENT_MESSAGE,
    spawn_background_command, to_base32, ApiMessage, AnthropicTool,
    BackgroundCommandParams, ChatEvent, Config, ContentBlock, McpPool,
    KEEPALIVE_INTERVAL, KEEPALIVE_MAX_MISSED,
    StreamState, buffer_and_fanout, chat_event_to_wire_json, messages_to_history,
    parse_ping_id, parse_pong_id, write_config,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Notify};
use serde::Deserialize;
use tower_http::cors::{Any, CorsLayer};

// ── Session persistence ───────────────────────────────────────────────────────

fn save_messages(messages: &[ApiMessage]) {
    okto_core::save_messages(&data_dir(), messages, "agent");
}

fn load_messages() -> Vec<ApiMessage> {
    okto_core::load_messages(&data_dir(), "agent")
}

/// Unified turn-gate: all decisions about who gets the next conversation turn
/// are made under this single lock, eliminating CAS races between
/// `is_streaming`, `pending_injections`, and user messages.
struct TurnGate {
    /// `true` while a streaming turn (user-driven or auto) is in progress.
    streaming:           bool,
    /// Background-task injections (`bg_complete` / `bg_progress`) waiting to
    /// be folded into `messages` at the next turn boundary.
    pending_injections:  Vec<ApiMessage>,
    /// User text queued when a turn was already running. Takes priority over
    /// auto-turn chaining — when present, the next turn is always user-driven.
    pending_user_msg:    Option<String>,
    /// Counts consecutive auto-turns. Reset to 0 on every user-driven turn.
    /// When it exceeds `MAX_AUTO_DEPTH`, further auto-turns are suppressed;
    /// injections are persisted to `messages` but the gate is released so
    /// the user can send a message.
    auto_depth:          u32,
    /// Set by `interrupt` frame. Prevents `finalize_turn` from chaining
    /// another auto-turn — injections are drained into `messages` and the
    /// gate is released, giving the user back control immediately.
    interrupt_requested: bool,
}

impl TurnGate {
    fn new() -> Self {
        Self {
            streaming:           false,
            pending_injections:  Vec::new(),
            pending_user_msg:    None,
            auto_depth:          0,
            interrupt_requested: false,
        }
    }
}

/// Maximum consecutive auto-turns before we stop chaining and release the gate.
const MAX_AUTO_DEPTH: u32 = 3;

// ── App state ─────────────────────────────────────────────────────────────────

struct AppState {
    messages:      Arc<Mutex<Vec<ApiMessage>>>,
    last_cost_usd: Mutex<Option<f64>>,
    system:        String,
    cwd:           String,
    stream_state:  Mutex<StreamState>,
    turn_gate:     Mutex<TurnGate>,
    cancel:        Mutex<CancellationToken>,
    mcp_pool:      McpPool,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse { (StatusCode::OK, "ok") }

async fn interrupt_handler(State(state): State<Arc<AppState>>) -> StatusCode {
    state.cancel.lock().unwrap().cancel();
    state.turn_gate.lock().unwrap().interrupt_requested = true;
    StatusCode::OK
}

async fn history_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cost = *state.last_cost_usd.lock().unwrap();
    let msgs = messages_to_history(&state.messages.lock().unwrap(), cost);
    Json(serde_json::json!({ "messages": msgs }))
}

async fn stream_handler(
    ws:           WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_stream(socket, state))
}

enum TurnTrigger { User(String), Auto }

fn spawn_turn(state: Arc<AppState>, trigger: TurnTrigger) {
    tokio::spawn(async move {
        let api_key = match resolve_api_key() {
            Some(k) => k,
            None => {
                error!("[agent/stream] no API key configured — aborting turn");
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
                {
                    let mut gate = state.turn_gate.lock().unwrap();
                    gate.streaming = false;
                    gate.auto_depth = 0;
                }
                return;
            }
        };
        let model = resolve_model();

        // Reset auto_depth on user-driven turns.
        if let TurnTrigger::User(text) = &trigger {
            state.turn_gate.lock().unwrap().auto_depth = 0;
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

        let cancel = CancellationToken::new();
        *state.cancel.lock().unwrap() = cancel.clone();

        state.stream_state.lock().unwrap().buffer.clear();

        let extra_tools = build_tools_with_mcp(&state.mcp_pool, &make_extra_tools()).await;
        let executor    = chain_executor_with_mcp(state.mcp_pool.clone(), make_extra_executor(state.clone()));

        tokio::spawn(async move {
            match send_message(messages, &system, &model, &api_key, &cwd, Some(event_tx), cancel.clone(), &extra_tools, executor).await {
                Ok((_, cost_usd, mut updated)) => {
                    if cancel.is_cancelled() {
                        info!("[agent/stream] turn interrupted, cost=${cost_usd:.4}");
                        updated.push(ApiMessage {
                            role:    "interrupted".to_string(),
                            content: vec![ContentBlock::Text { text: "interrupted".to_string() }],
                        });
                        *msgs_arc.lock().unwrap() = updated.clone();
                        save_messages(&updated);
                        *state_arc.last_cost_usd.lock().unwrap() = Some(cost_usd);
                        done_tx.send(ChatEvent::Interrupted { cost_usd }).await.ok();
                    } else {
                        info!("[agent/stream] turn finished, cost=${cost_usd:.4}");
                        *msgs_arc.lock().unwrap() = updated.clone();
                        save_messages(&updated);
                        *state_arc.last_cost_usd.lock().unwrap() = Some(cost_usd);
                        done_tx.send(ChatEvent::Result {
                            cost_usd, turns: 0, session_id: String::new(), result: None,
                        }).await.ok();
                    }
                }
                Err((e, mut partial)) => {
                    error!("[agent/stream] turn failed: {e}");
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

        while let Some(event) = event_rx.recv().await {
            if let Some(json) = chat_event_to_wire_json(&event) {
                buffer_and_fanout(&state.stream_state, json.to_string());
            }
        }
        state.stream_state.lock().unwrap().buffer.clear();
        // Decide next turn under a single lock: drain injections / queued user
        // message, then either chain an auto-turn, start a user-driven turn, or
        // release the gate.
        finalize_turn(state.clone());
    });
}

/// End-of-turn handoff: drains any queued background-task injections, checks
/// for a pending user message, and decides what to do next.
///
/// All decisions are made under the single `turn_gate` lock so there are no
/// CAS races. The lock is held for microseconds (just the decision), not
/// during the turn itself. Priority order:
///
///   1. Queued user message → user-driven turn (always wins)
///   2. Queued injections → auto-turn (only if no user message,
///      interrupt was not requested, and auto_depth < MAX_AUTO_DEPTH)
///   3. Nothing → release the gate (streaming = false)
fn finalize_turn(state: Arc<AppState>) {
    let (injections, user_msg, should_auto) = {
        let mut gate = state.turn_gate.lock().unwrap();
        let injections = std::mem::take(&mut gate.pending_injections);
        let user_msg   = gate.pending_user_msg.take();
        let should_auto = !gate.interrupt_requested
            && gate.auto_depth < MAX_AUTO_DEPTH
            && user_msg.is_none()
            && !injections.is_empty();
        gate.interrupt_requested = false;
        if should_auto {
            gate.auto_depth += 1;
            // streaming stays true — we chain into an auto-turn.
        } else if let Some(_) = &user_msg {
            // streaming stays true — we chain into a user-driven turn.
            gate.auto_depth = 0;
        } else {
            gate.streaming = false;
            gate.auto_depth = 0;
        }
        drop(gate);
        (injections, user_msg, should_auto)
    };

    // Fold injections + queued user message into the persisted history.
    // Capture `had_injections` first because `msgs.extend` consumes the Vec.
    let had_injections = !injections.is_empty();
    {
        let mut msgs = state.messages.lock().unwrap();
        if let Some(text) = &user_msg {
            msgs.push(ApiMessage {
                role:    "user".to_string(),
                content: vec![ContentBlock::Text { text: text.clone() }],
            });
        }
        msgs.extend(injections);
        save_messages(&msgs);
    }

    if let Some(text) = user_msg {
        info!("[agent/stream] queued user message takes priority — spawning user-driven turn");
        spawn_turn(state, TurnTrigger::User(text));
        return;
    }

    if should_auto {
        info!("[agent/stream] auto-turn triggered by queued background injection");
        spawn_turn(state, TurnTrigger::Auto);
        return;
    }

    if had_injections {
        info!("[agent/stream] injections persisted but auto-turn suppressed (interrupt or depth cap)");
    } else {
        info!("[agent/stream] turn complete, gate released");
    }
}

/// Drain any queued background-task injections into the conversation and spawn
/// an auto-turn so the model reacts. Called from the monitor loop and from
/// `run_tracked_command`'s deliver closure when they produce new output.
///
/// All decisions are made under the single `turn_gate` lock. If a turn is
/// already running, the injection is left in `pending_injections` and the
/// currently-running turn's `finalize_turn` will drain it once it finishes.
/// If a user message is queued, the injection is added to `pending_injections`
/// but no auto-turn is spawned — the queued user message takes priority.
fn try_continue_auto(state: Arc<AppState>) {
    let should_spawn = {
        let mut gate = state.turn_gate.lock().unwrap();
        if gate.pending_injections.is_empty() { return; }
        if gate.streaming {
            // Turn already running — leave the injection queued.
            return;
        }
        // A queued user message takes priority — don't spawn an auto-turn.
        // The injection stays in `pending_injections`; `finalize_turn` (or
        // the next user-message handler) will fold it in.
        if gate.pending_user_msg.is_some() { return; }
        if gate.interrupt_requested { return; }
        if gate.auto_depth >= MAX_AUTO_DEPTH { return; }
        gate.streaming = true;
        gate.auto_depth += 1;
        true
    };
    if !should_spawn { return; }

    // Drain injections into persisted messages.
    let drained: Vec<ApiMessage> = {
        let mut gate = state.turn_gate.lock().unwrap();
        std::mem::take(&mut gate.pending_injections)
    };
    if drained.is_empty() {
        // Lost the race — another drain emptied the queue between our check
        // and the drain. Release the gate.
        let mut gate = state.turn_gate.lock().unwrap();
        gate.streaming = false;
        gate.auto_depth = 0;
        return;
    }
    {
        let mut msgs = state.messages.lock().unwrap();
        msgs.extend(drained);
        save_messages(&msgs);
    }
    info!("[agent/stream] auto-turn triggered by queued background injection");
    spawn_turn(state, TurnTrigger::Auto);
}

async fn handle_stream(socket: WebSocket, state: Arc<AppState>) {
    info!("[agent/stream] WebSocket connection opened");
    let (mut ws_tx, mut ws_rx) = socket.split();

    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<String>();
    let (replay, resumed) = {
        let mut ss = state.stream_state.lock().unwrap();
        ss.subs.push(sub_tx);
        let resumed = state.turn_gate.lock().unwrap().streaming;
        let replay = if resumed { ss.buffer.clone() } else { Vec::new() };
        (replay, resumed)
    };

    let ready = serde_json::json!({"type":"ready","session_id":"","resumed":resumed}).to_string();
    if ws_tx.send(WsMessage::Text(ready)).await.is_err() {
        debug!("[agent/stream] client disconnected before ready frame");
        return;
    }
    if ws_tx.send(WsMessage::Text(tasks_wire_json(&state.stream_state))).await.is_err() {
        debug!("[agent/stream] client disconnected before tasks frame");
        return;
    }
    if !replay.is_empty() {
        info!("[agent/stream] replaying {} buffered event(s) to new connection", replay.len());
        for event in replay {
            if ws_tx.send(WsMessage::Text(event)).await.is_err() { return; }
        }
    }

    let mut ping_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + KEEPALIVE_INTERVAL,
        KEEPALIVE_INTERVAL,
    );
    let mut next_ping_id:  u64 = 0;
    let mut last_acked_id: u64 = 0;

    loop {
        tokio::select! {
            msg = sub_rx.recv() => match msg {
                Some(json) => {
                    if ws_tx.send(WsMessage::Text(json)).await.is_err() { break; }
                }
                None => break,
            },

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

            msg = ws_rx.next() => match msg {
                Some(Ok(WsMessage::Text(t))) => {
                    if let Some(id) = parse_pong_id(&t) {
                        if id > last_acked_id { last_acked_id = id; }
                    } else if let Some(id) = parse_ping_id(&t) {
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
            {
                let mut gate = state.turn_gate.lock().unwrap();
                if gate.streaming {
                    // A turn is already running. Queue the user message so it
                    // takes priority in the next `finalize_turn` call.
                    if gate.pending_user_msg.is_some() {
                        // Already have a queued message — can only hold one.
                        let json = serde_json::json!({"type":"error","message":"a message is already queued, please wait"}).to_string();
                        buffer_and_fanout(&state.stream_state, json);
                    } else {
                        gate.pending_user_msg = Some(text.clone());
                        let preview: String = text.chars().take(120).collect();
                        info!("[agent/stream] user_message queued (turn running): {preview}");
                        let json = serde_json::json!({"type":"queued","text_preview": preview}).to_string();
                        buffer_and_fanout(&state.stream_state, json);
                    }
                    // Don't spawn a turn — the running turn's finalize_turn
                    // will pick up the queued message.
                    return;
                }
                // Gate is free — claim it for a user-driven turn.
                gate.streaming = true;
                gate.auto_depth = 0;
            }
            let preview: String = text.chars().take(120).collect();
            info!("[agent/stream] user_message ({} chars): {preview}", text.len());
            spawn_turn(state.clone(), TurnTrigger::User(text));
        }
        "interrupt" => {
            info!("[agent/stream] interrupt frame received");
            state.cancel.lock().unwrap().cancel();
            state.turn_gate.lock().unwrap().interrupt_requested = true;
            buffer_and_fanout(&state.stream_state, serde_json::json!({"type":"interrupt_ack"}).to_string());
        }
        "cancel_task" => {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if id.is_empty() {
                warn!("[agent/stream] cancel_task frame missing id");
                return;
            }
            let fired = core_cancel_task(&state.stream_state, &id);
            info!("[agent/stream] cancel_task id={id} fired={fired}");
            buffer_and_fanout(
                &state.stream_state,
                serde_json::json!({"type":"cancel_task_ack","id":id,"fired":fired}).to_string(),
            );
        }
        "pong" => {}
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
    if !PathBuf::from(&state.cwd).join(".git").is_dir() {
        return Json(Vec::<okto_core::Branch>::new()).into_response();
    }
    match get_branches_for_repo(&state.cwd) {
        Ok(b)  => Json(b).into_response(),
        Err(e) => {
            warn!("[agent/branches] failed to list branches for {}: {e}", state.cwd);
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

async fn get_config_handler() -> Json<Config> { Json(read_config()) }

async fn update_config_handler(Json(patch): Json<Config>) -> StatusCode {
    info!(
        "[agent/config] update model={:?} anthropic_api_key={}",
        patch.model,
        if patch.anthropic_api_key.is_some() { "provided" } else { "unchanged" }
    );
    let mut cfg = read_config();
    if patch.anthropic_api_key.is_some() { cfg.anthropic_api_key = patch.anthropic_api_key; }
    if patch.model.is_some()             { cfg.model             = patch.model; }
    write_config(&cfg);
    StatusCode::OK
}

fn make_extra_tools() -> Vec<AnthropicTool> {
    let mut tools = vec![
        run_command_in_background_tool(),
        monitor_process_tool(),
        send_notification_tool(),
    ];
    if has_spawn_capability() {
        tools.push(spawn_agent_tool());
        tools.push(terminate_agent_tool());
    }
    tools
}

/// True when lair handed this child a capability token at spawn time. Only
/// agent-spawned children get one; operator-spawned (top-level) children do
/// not, and therefore can't see the `spawn_agent` / `terminate_agent` tools.
fn has_spawn_capability() -> bool {
    std::env::var("OKTO_AGENT_TOKEN").ok().filter(|s| !s.is_empty()).is_some()
        && std::env::var("LAIR_INTERNAL_URL").ok().filter(|s| !s.is_empty()).is_some()
}

fn spawn_agent_tool() -> AnthropicTool {
    AnthropicTool {
        name: "spawn_agent".to_string(),
        description: "Spawn a new okto child agent owned by this agent. The new agent runs as \
                       a separate OS process inside the lair container with its own loopback \
                       port and per-agent uid. You can terminate any agent you spawn (or any \
                       transitive descendant) with `terminate_agent`. Operator caps may refuse \
                       this call if you've reached the maximum spawn depth or descendant count."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "git_url": {
                    "type": "string",
                    "description": "Optional Git repository to clone into the new agent's workspace at spawn time."
                },
                "name": {
                    "type": "string",
                    "description": "Optional logical name for the new child. Defaults to lair-<repo-slug> if git_url is set, else lair-workload."
                },
                "startup_script": {
                    "type": "string",
                    "description": "Optional shell script run inside the child before its HTTP server starts. Never include secrets."
                },
                "startup_prompt": {
                    "type": "string",
                    "description": "Optional first user message to the child's agentic loop once ready. Never include secrets."
                },
                "mcp": {
                    "type": "array",
                    "description": "Optional MCP server list for the new child. OMIT to inherit lair's current mcp.json verbatim (the default — the child gets the same MCP tools lair has). Pass an empty array [] to give the child no MCP servers. Pass a non-empty array to override with exactly these servers — each entry matches the mcp.json schema: {name, command, args?, env?} for stdio or {name, url, headers?} for HTTP. The list is snapshotted into the child's data dir at spawn time.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name":    { "type": "string" },
                            "command": { "type": "string" },
                            "args":    { "type": "array", "items": { "type": "string" } },
                            "env":     { "type": "object", "additionalProperties": { "type": "string" } },
                            "url":     { "type": "string" },
                            "headers": { "type": "object", "additionalProperties": { "type": "string" } }
                        },
                        "required": ["name"]
                    }
                }
            },
            "required": []
        }),
        display_label: Some("Spawning agent".into()),
    }
}

fn terminate_agent_tool() -> AnthropicTool {
    AnthropicTool {
        name: "terminate_agent".to_string(),
        description: "Permanently terminate an agent that this agent spawned (or any transitive \
                       descendant). Kills the process, cascade-terminates everything beneath it, \
                       and deletes the per-agent data + workspace directories. Refuses non-\
                       descendant names. Irreversible."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Name of the descendant agent to terminate." }
            },
            "required": ["name"]
        }),
        display_label: Some("Terminating agent".into()),
    }
}

fn make_extra_executor(state: Arc<AppState>) -> Option<Arc<dyn Fn(String, serde_json::Value)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
    + Send + Sync>>
{
    Some(Arc::new(move |name: String, input: serde_json::Value| {
        let state  = state.clone();
        Box::pin(async move {
            match name.as_str() {
                "run_command_in_background" => exec_run_command_in_background(state, input).await,
                "monitor_process"           => exec_monitor_process(state, input).await,
                "spawn_agent"               => exec_spawn_agent(input).await,
                "terminate_agent"           => exec_terminate_agent(input).await,
                "send_notification"         => exec_send_notification(input).await,
                other => format!("unknown tool: {other}"),
            }
        })
    }))
}

async fn exec_spawn_agent(input: serde_json::Value) -> String {
    let Some(token) = std::env::var("OKTO_AGENT_TOKEN").ok().filter(|s| !s.is_empty()) else {
        warn!("[agent/spawn_agent] refused: no OKTO_AGENT_TOKEN in env");
        return "error: this agent has no spawn capability (no OKTO_AGENT_TOKEN in env).".to_string();
    };
    let Some(base)  = std::env::var("LAIR_INTERNAL_URL").ok().filter(|s| !s.is_empty()) else {
        warn!("[agent/spawn_agent] refused: LAIR_INTERNAL_URL unset");
        return "error: LAIR_INTERNAL_URL is unset; cannot reach lair management API.".to_string();
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c)  => c,
        Err(e) => {
            error!("[agent/spawn_agent] build http client failed: {e}");
            return format!("error: build http client: {e}");
        }
    };
    let url = format!("{base}/agents/child");
    info!("[agent/spawn_agent] requesting child spawn via {url}");
    let resp = match client.post(&url)
        .header("X-Okto-Agent-Token", token)
        .json(&input)
        .send()
        .await
    {
        Ok(r)  => r,
        Err(e) => {
            error!("[agent/spawn_agent] POST {url} failed: {e}");
            return format!("error: POST {url}: {e}");
        }
    };
    let status = resp.status();
    let body   = resp.text().await.unwrap_or_default();
    if status.is_success() {
        info!("[agent/spawn_agent] child spawn succeeded");
        body
    } else {
        warn!("[agent/spawn_agent] child spawn rejected ({status})");
        format!("error ({status}): {body}")
    }
}

async fn exec_terminate_agent(input: serde_json::Value) -> String {
    let name = match input.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return "error: missing 'name' field".to_string(),
    };
    let Some(token) = std::env::var("OKTO_AGENT_TOKEN").ok().filter(|s| !s.is_empty()) else {
        warn!("[agent/terminate_agent] refused: no OKTO_AGENT_TOKEN in env");
        return "error: this agent has no terminate capability (no OKTO_AGENT_TOKEN in env).".to_string();
    };
    let Some(base)  = std::env::var("LAIR_INTERNAL_URL").ok().filter(|s| !s.is_empty()) else {
        warn!("[agent/terminate_agent] refused: LAIR_INTERNAL_URL unset");
        return "error: LAIR_INTERNAL_URL is unset; cannot reach lair management API.".to_string();
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c)  => c,
        Err(e) => {
            error!("[agent/terminate_agent] build http client failed: {e}");
            return format!("error: build http client: {e}");
        }
    };
    let url = format!("{base}/agents/child/{name}");
    info!("[agent/terminate_agent] requesting termination of '{name}' via {url}");
    let resp = match client.delete(&url)
        .header("X-Okto-Agent-Token", token)
        .send()
        .await
    {
        Ok(r)  => r,
        Err(e) => {
            error!("[agent/terminate_agent] DELETE {url} failed: {e}");
            return format!("error: DELETE {url}: {e}");
        }
    };
    let status = resp.status();
    let body   = resp.text().await.unwrap_or_default();
    if status.is_success() {
        info!("[agent/terminate_agent] '{name}' terminated");
        format!("Terminated '{name}' and any descendants.")
    } else {
        warn!("[agent/terminate_agent] termination of '{name}' rejected ({status})");
        format!("error ({status}): {body}")
    }
}

/// Forward a push-notification request to lair. Child agents hold no relay
/// signing key (mobile is subscribed under lair's pubkey), so lair signs and
/// forwards to the relay on the child's behalf via its container-internal
/// `/internal/notify` endpoint. Best-effort: every failure is logged and
/// swallowed — a missing push must never disturb the agentic loop.
async fn forward_notify_to_lair(category: &str, title: &str, body: &str) {
    let Some(base) = std::env::var("LAIR_INTERNAL_URL").ok().filter(|s| !s.is_empty()) else {
        warn!("[agent/notify] LAIR_INTERNAL_URL unset — cannot forward push");
        return;
    };
    // Prefix the title with this agent's name so the operator can tell which
    // agent a push came from. `AGENT_NAME` is set by the supervisor at spawn.
    let title = match std::env::var("AGENT_NAME").ok().filter(|s| !s.is_empty()) {
        Some(name) => format!("{name} · {title}"),
        None       => title.to_string(),
    };
    let url = format!("{base}/internal/notify");
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c)  => c,
        Err(e) => { warn!("[agent/notify] build http client failed: {e}"); return; }
    };
    let payload = serde_json::json!({ "category": category, "title": title, "body": body });
    match client.post(&url).json(&payload).send().await {
        Ok(r) if r.status().is_success() => debug!("[agent/notify] forwarded push to lair ({})", r.status()),
        Ok(r)  => warn!("[agent/notify] lair {url} returned {}", r.status()),
        Err(e) => warn!("[agent/notify] POST {url} failed: {e}"),
    }
}

/// `send_notification` tool — a child agent holds no relay signing key, so it
/// forwards the push to lair, which signs and relays it on the agent's behalf.
async fn exec_send_notification(input: serde_json::Value) -> String {
    let title = input.get("title").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let body  = input.get("body").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if body.is_empty() {
        return "error: 'body' is required".to_string();
    }
    if std::env::var("LAIR_INTERNAL_URL").ok().filter(|s| !s.is_empty()).is_none() {
        warn!("[agent/send_notification] LAIR_INTERNAL_URL unset — push dropped");
        return "Notification not sent: this agent cannot reach lair to relay the push.".to_string();
    }
    forward_notify_to_lair(NOTIFY_CATEGORY_AGENT_MESSAGE, &title, &body).await;
    info!("[agent/send_notification] forwarded push to lair");
    "Notification dispatched to the operator's device.".to_string()
}

async fn exec_run_command_in_background(state: Arc<AppState>, input: serde_json::Value) -> String {
    let command = match input.get("command").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return "error: missing or empty 'command'".to_string(),
    };

    let task_id = format!("bg-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    info!("[agent/run_command_in_background] spawning {task_id} ({} chars)", command.len());

    let cancel = CancellationToken::new();
    let output = register_task(&state.stream_state, &data_dir(), TaskRecord {
        task_id:      task_id.clone(),
        command:      command.clone(),
        status:       TaskStatus::Running,
        started_at:   now_secs(),
        completed_at: None,
        summary:      None,
        cost_usd:     None,
        wake_interval_secs: None,
    }, cancel.clone());
    buffer_and_fanout(&state.stream_state, tasks_wire_json(&state.stream_state));

    run_tracked_command(state, task_id.clone(), command, cancel, output);
    format!("Background command {task_id} started. The user will be notified when it completes.")
}

/// Spawn a registered background task and wire up the standard completion
/// handling: finalize the registry row, fan out the `bg_complete` event, queue
/// the `bg_complete` injection, fire a push, and kick `try_continue_auto`.
fn run_tracked_command(
    state:   Arc<AppState>,
    task_id: String,
    command: String,
    cancel:  CancellationToken,
    output:  Arc<Mutex<TaskOutput>>,
) {
    let params = BackgroundCommandParams {
        task_id,
        command,
        cwd: state.cwd.clone(),
    };
    let deliver_state = state.clone();
    spawn_background_command(params, cancel, output, move |outcome| {
        finalize_task(&deliver_state.stream_state, &data_dir(), &outcome);
        buffer_and_fanout(&deliver_state.stream_state, tasks_wire_json(&deliver_state.stream_state));

        let injection = format!(
            "Background command {} completed (status={}). Command: {}\n\nOutput:\n{}",
            outcome.task_id, outcome.status, outcome.command, outcome.summary
        );
        deliver_state.turn_gate.lock().unwrap().pending_injections.push(ApiMessage {
            role:    "bg_complete".to_string(),
            content: vec![ContentBlock::Text { text: injection.clone() }],
        });

        let bg_event = ChatEvent::BgComplete {
            task_id: outcome.task_id.clone(),
            text:    injection,
        };
        if let Some(json) = chat_event_to_wire_json(&bg_event) {
            buffer_and_fanout(&deliver_state.stream_state, json.to_string());
        }

        let event = completion_chat_event(&outcome);
        if let Some(json) = chat_event_to_wire_json(&event) {
            buffer_and_fanout(&deliver_state.stream_state, json.to_string());
        }

        // Push notification. Children have no relay key, so lair signs and
        // forwards on our behalf — see `forward_notify_to_lair`.
        let title = format!("Background command {}", outcome.status);
        let body  = outcome.summary.chars().take(120).collect::<String>();
        tokio::spawn(async move {
            forward_notify_to_lair("task_complete", &title, &body).await;
        });

        try_continue_auto(deliver_state.clone());
    });
}

async fn exec_monitor_process(state: Arc<AppState>, input: serde_json::Value) -> String {
    let command = match input.get("command").and_then(|v| v.as_str()).map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return "error: missing or empty 'command'".to_string(),
    };
    let purpose = input.get("purpose").and_then(|v| v.as_str())
        .map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let interval = input.get("wake_interval_secs").and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_WAKE_INTERVAL_SECS)
        .max(MIN_WAKE_INTERVAL_SECS);

    let task_id = format!("bg-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    info!("[agent/monitor_process] spawning {task_id} ({} chars) interval={interval}s", command.len());
    let cancel = CancellationToken::new();
    let output = register_task(&state.stream_state, &data_dir(), TaskRecord {
        task_id:      task_id.clone(),
        command:      command.clone(),
        status:       TaskStatus::Running,
        started_at:   now_secs(),
        completed_at: None,
        summary:      None,
        cost_usd:     None,
        wake_interval_secs: Some(interval),
    }, cancel.clone());
    buffer_and_fanout(&state.stream_state, tasks_wire_json(&state.stream_state));
    let label = purpose.unwrap_or_else(|| command.clone());

    let done = Arc::new(Notify::new());
    run_monitored_command(state.clone(), task_id.clone(), command, cancel.clone(), output, done.clone());
    spawn_monitor(state, task_id.clone(), label, interval, cancel, done);
    format!("Monitoring background process {task_id}. You'll be woken with new output \
             roughly every {interval}s while it runs, and once more when it exits.")
}

/// Run a monitored task's process. Unlike `run_tracked_command`, completion
/// does not wake the model itself — it finalizes the registry row and fires a
/// push, then signals `done` so the attached monitor loop delivers the single
/// final wake-up. This keeps the whole lifecycle owned by the monitor.
fn run_monitored_command(
    state:   Arc<AppState>,
    task_id: String,
    command: String,
    cancel:  CancellationToken,
    output:  Arc<Mutex<TaskOutput>>,
    done:    Arc<Notify>,
) {
    let params = BackgroundCommandParams {
        task_id,
        command,
        cwd: state.cwd.clone(),
    };
    let deliver_state = state.clone();
    spawn_background_command(params, cancel, output, move |outcome| {
        finalize_task(&deliver_state.stream_state, &data_dir(), &outcome);
        buffer_and_fanout(&deliver_state.stream_state, tasks_wire_json(&deliver_state.stream_state));
        // Push notification. Children have no relay key, so lair signs and
        // forwards on our behalf — see `forward_notify_to_lair`.
        let title = format!("Monitored process {}", outcome.status);
        let body  = outcome.summary.chars().take(120).collect::<String>();
        tokio::spawn(async move {
            forward_notify_to_lair("task_complete", &title, &body).await;
        });
        done.notify_one();
    });
}

/// Detached loop that wakes the model with a monitored task's output: with new
/// output at most every `interval`s while it runs, and once more when it
/// exits. Stops on completion or cancellation.
fn spawn_monitor(
    state:    Arc<AppState>,
    task_id:  String,
    label:    String,
    interval: u64,
    cancel:   CancellationToken,
    done:     Arc<Notify>,
) {
    tokio::spawn(async move {
        let period = Duration::from_secs(interval);
        let mut cursor = 0usize;
        info!("[agent/monitor] watching {task_id} every {interval}s");
        loop {
            let exited = tokio::select! {
                _ = tokio::time::sleep(period) => false,
                _ = done.notified()            => true,
                _ = cancel.cancelled() => {
                    info!("[agent/monitor] {task_id} cancelled, stopping");
                    return;
                }
            };
            let (output, status) = {
                let ss = state.stream_state.lock().unwrap();
                let output = ss.task_outputs.get(&task_id).cloned();
                let status = ss.tasks.iter().find(|t| t.task_id == task_id).map(|t| t.status);
                (output, status)
            };
            let Some(output) = output else {
                info!("[agent/monitor] {task_id} buffer gone, stopping");
                return;
            };
            let (new_text, new_cursor) = output.lock().unwrap().read_since(cursor);
            cursor = new_cursor;
            if exited || !matches!(status, Some(TaskStatus::Running)) {
                let status_str = match status {
                    Some(TaskStatus::Done)      => "done",
                    Some(TaskStatus::Error)     => "error",
                    Some(TaskStatus::Cancelled) => "cancelled",
                    _                           => "ended",
                };
                state.turn_gate.lock().unwrap().pending_injections
                    .push(monitor_complete_message(&task_id, &label, status_str, &new_text));
                let ev = ChatEvent::BgComplete {
                    task_id: task_id.clone(),
                    text:    monitor_complete_text(&task_id, &label, status_str, &new_text),
                };
                if let Some(json) = chat_event_to_wire_json(&ev) {
                    buffer_and_fanout(&state.stream_state, json.to_string());
                }
                info!("[agent/monitor] {task_id} finished ({status_str}), stopping");
                try_continue_auto(state.clone());
                return;
            }
            if !new_text.trim().is_empty() {
                state.turn_gate.lock().unwrap().pending_injections
                    .push(monitor_progress_message(&task_id, &label, &new_text));
                let ev = ChatEvent::BgProgress {
                    task_id: task_id.clone(),
                    text:    monitor_progress_text(&task_id, &label, &new_text),
                };
                if let Some(json) = chat_event_to_wire_json(&ev) {
                    buffer_and_fanout(&state.stream_state, json.to_string());
                }
                try_continue_auto(state.clone());
            }
        }
    });
}

// ── Agent identity ────────────────────────────────────────────────────────────

/// Write `<data_dir>/agent-info.json` with this agent's externally-visible
/// identity. Idempotent — overwritten on each boot. Used by remote-agent
/// registration: lair SSH-reads this file to learn the agent's Noise pubkey
/// + public port after cloud-init completes.
fn write_agent_info(dir: &std::path::Path, pubkey_b32: &str, public_port: u16) -> std::io::Result<()> {
    let info = serde_json::json!({
        "pubkey":   pubkey_b32,
        "port":     public_port,
        "ready_at": okto_core::now_secs(),
    });
    let path = dir.join("agent-info.json");
    std::fs::create_dir_all(dir).ok();
    std::fs::write(&path, serde_json::to_string_pretty(&info).unwrap_or_default())
}

// ── Entry ─────────────────────────────────────────────────────────────────────

pub async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    init_shell_env();

    let port: u16 = std::env::var("AGENT_PORT")
        .ok().and_then(|v| v.parse().ok())
        .unwrap_or(30100);

    // Optional Noise responder for remote agents. The cloud-init userdata
    // produced by `mint_bootstrap_userdata` sets `AGENT_NOISE_PORT` so the
    // VM exposes the agent over a Noise-encrypted public port; local agents
    // leave it unset and bind only on the loopback HTTP port (lair reaches
    // them on 127.0.0.1).
    let agent_noise_port: Option<u16> = std::env::var("AGENT_NOISE_PORT")
        .ok().and_then(|v| v.parse().ok());

    info!("[agent] starting on 127.0.0.1:{port} (noise_port={agent_noise_port:?})");

    if let Some(noise_port) = agent_noise_port {
        let key_file = std::env::var("NOISE_KEY_FILE")
            .unwrap_or_else(|_| data_dir().join("noise_key.bin").to_string_lossy().to_string());
        let (static_private, static_public) = load_or_generate_keypair(&key_file);
        let pubkey_b32 = to_base32(&static_public);
        info!("[agent] noise_pubkey={pubkey_b32}");
        if let Err(e) = write_agent_info(&data_dir(), &pubkey_b32, noise_port) {
            warn!("[agent] could not write agent-info.json: {e}");
        }
        // Initiator-pubkey allowlist: only lair (whose static pubkey is
        // embedded in the userdata as `LAIR_PUBKEY=<base32>`) is allowed to
        // complete the Noise XX handshake to this responder. Without this,
        // anyone on the internet who learned `(host, port, agent_pubkey)`
        // could speak the agent's protocol — Noise XX proves possession of
        // a static key but doesn't enforce identity. Fail-closed: if
        // `LAIR_PUBKEY` is unset or malformed for a remote agent, refuse
        // to start the responder rather than running open.
        let expected_lair_pubkey: Option<Vec<u8>> = match std::env::var("LAIR_PUBKEY") {
            Ok(s) if !s.is_empty() => match from_base32(&s) {
                Some(bytes) if bytes.len() == 32 => Some(bytes),
                Some(_) => {
                    return Err(anyhow::anyhow!(
                        "LAIR_PUBKEY decodes to non-32-byte value; expected a 32-byte Curve25519 public key"
                    ));
                }
                None => {
                    return Err(anyhow::anyhow!(
                        "LAIR_PUBKEY is not valid base32 (RFC 4648 no-pad)"
                    ));
                }
            },
            _ => {
                return Err(anyhow::anyhow!(
                    "AGENT_NOISE_PORT is set but LAIR_PUBKEY is missing — refusing to expose an unauthenticated Noise responder. Re-mint userdata via `mint_bootstrap_userdata` so it embeds lair's pubkey."
                ));
            }
        };
        tokio::spawn(run_noise_proxy(static_private, noise_port, port, expected_lair_pubkey));
    }

    let workspace = std::path::PathBuf::from(
        std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "workspace".to_string())
    );
    let git_url  = std::env::var("GIT_URL").ok();
    let gh_token = std::env::var("GH_TOKEN").ok();
    let has_repo = crate::bootstrap::ensure_workspace(
        &workspace,
        git_url.as_deref(),
        gh_token.as_deref(),
    ).await?;

    crate::bootstrap::run_startup_script("agent").await?;

    // The container's shared SSH keypair has already been seeded into
    // `$HOME/.ssh/id_ed25519{,.pub}` by lair's `AgentSupervisor::spawn`
    // (see `lair/src/agent_proc.rs`), so any tool call inside the agent —
    // raw `ssh user@host`, `git push`, etc. — uses the same identity as
    // every other process in this container, with no per-agent setup.

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
        stream_state:  Mutex::new({
            let mut ss = StreamState::new();
            ss.tasks = okto_core::load_tasks(&data_dir(), "agent");
            ss
        }),
        turn_gate:    Mutex::new(TurnGate::new()),
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
        .route("/stream",      get(stream_handler))
        .route("/interrupt",   post(interrupt_handler))
        .route("/clear",       post(clear_handler))
        .route("/branches",    get(get_branches_handler))
        .route("/completions", get(get_completions_handler))
        .route("/config",      get(get_config_handler).put(update_config_handler))
        .with_state(state.clone())
        .layer(cors);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await
        .map_err(|e| {
            error!("[agent] failed to bind agent HTTP port {addr}: {e}");
            anyhow::anyhow!("failed to bind agent HTTP port {addr}: {e}")
        })?;
    info!("[agent] HTTP listening on {addr} (cwd: {})", state.cwd);

    if let Ok(prompt) = std::env::var("STARTUP_PROMPT") {
        if !prompt.is_empty() {
            let state_sp   = Arc::clone(&state);
            let api_key_sp = resolve_api_key().unwrap_or_default();
            let model_sp   = resolve_model();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                info!("[agent] running STARTUP_PROMPT ({} chars)", prompt.len());
                {
                    let mut gate = state_sp.turn_gate.lock().unwrap();
                    gate.streaming = true;
                    gate.auto_depth = 0;
                }
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
                // Release the gate after the startup prompt turn.
                let mut gate = state_sp.turn_gate.lock().unwrap();
                gate.streaming = false;
                gate.auto_depth = 0;
            });
        }
    }

    axum::serve(listener, app).await
        .map_err(|e| anyhow::anyhow!("axum serve error: {e}"))?;
    Ok(())
}
