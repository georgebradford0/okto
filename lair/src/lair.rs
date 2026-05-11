use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

use tracing::{debug, error, info, warn};

use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bollard::Docker;
use octo_core::{
    self,
    build_tools_with_mcp, chain_executor_with_mcp,
    cancel_task as core_cancel_task, completion_chat_event, ensure_ssh_keypair, finalize_task,
    init_mcp_pool, init_shell_env, load_or_generate_keypair, now_secs, register_task,
    tasks_wire_json, TaskRecord, TaskStatus,
    relay as relay_client, RelaySigner,
    resolve_api_key, resolve_model, run_noise_proxy, run_background_task_tool, send_message,
    spawn_background_task, to_base32, ApiMessage, AnthropicTool, BackgroundTaskParams, ChatEvent,
    ContentBlock, McpPool, DEV_PUBKEY_BASE32, DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC,
    KEEPALIVE_INTERVAL, KEEPALIVE_MAX_MISSED,
    StreamState, buffer_and_fanout, chat_event_to_wire_json, messages_to_history,
    parse_ping_id, parse_pong_id,
};
use hex;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch, Notify};
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};

use crate::docker as docker_ops;
use crate::ssh as ssh_ops;
use octo_core::{AgentRecord, AgentStatus, Registry, status_from_docker};

// ── Noise Protocol ────────────────────────────────────────────────────────────

const NOISE_KEY_FILE:         &str = "/data/noise_key.bin";
const RELAY_SIGNING_KEY_FILE: &str = "/data/relay_signing_key.bin";
const DEFAULT_RELAY_URL:      &str = "https://octorelay.directto.link";

// ── Container registry ────────────────────────────────────────────────────────

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("OCTO_DATA_DIR") {
        PathBuf::from(d)
    } else {
        PathBuf::from("/data")
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

// ── Session persistence ───────────────────────────────────────────────────────
//
// Thin local wrappers that bind the shared `octo_core::app` helpers to this
// binary's data dir and log prefix.

fn save_messages(messages: &[ApiMessage]) {
    octo_core::save_messages(&data_dir(), messages, "lair");
}

fn load_messages() -> Vec<ApiMessage> {
    octo_core::load_messages(&data_dir(), "lair")
}

// ── App state ─────────────────────────────────────────────────────────────────

struct AppState {
    messages:             Arc<Mutex<Vec<ApiMessage>>>,
    last_cost_usd:        Mutex<Option<f64>>,
    system:               String,
    /// Watch channel published by the Docker poller. Each /stream WS subscribes
    /// and re-sends a `containers` event whenever the list changes.
    containers_tx:        watch::Sender<Vec<ContainerInfo>>,
    /// Receiver kept alongside the sender so `containers_tx` always has at
    /// least one subscriber (avoids `send` failures when no WS is open).
    containers_rx:        watch::Receiver<Vec<ContainerInfo>>,
    poll_trigger:         Arc<Notify>,
    pubkey_b32:           String,
    /// Hex-encoded 64-byte keypair (32 private + 32 public); injected into children.
    noise_private_key_hex: String,
    public_host:          String,
    /// Local Docker daemon client. Cheap to clone.
    docker:               Arc<Docker>,
    /// Source-of-truth list of agents lair owns. Persisted to
    /// `<data_dir>/agents.json`. Docker is the runtime source of truth for
    /// status; the poller reconciles the two.
    registry:             Arc<Mutex<Registry>>,
    mcp_pool:              McpPool,
    /// Cancellation token for the current streaming turn. Replaced at the start of each turn.
    cancel:               Mutex<CancellationToken>,
    /// True while an agentic turn is actively running. Guards against concurrent
    /// `user_message` frames; the second one is rejected until the first completes.
    is_streaming:         AtomicBool,
    /// Buffered events for the current turn + live subscriber list. Late /stream
    /// joiners replay the buffer so they don't miss events emitted before they
    /// connected. Cleared at the start of each new turn.
    stream_state:         Mutex<StreamState>,
    /// Flips to true once subsystem initialization completes (first containers
    /// poll done). `handle_stream` waits on this before emitting `ready`, so
    /// mobile keeps showing "connecting" during a reload window instead of
    /// flashing "live" against a half-initialized server.
    ready_rx:             watch::Receiver<bool>,
    /// Ed25519 keypair used to sign push-notification relay POSTs. Public half
    /// is exposed via `/info` so mobile can register it with the relay over
    /// the encrypted Noise tunnel before any relay traffic flows.
    relay_signer:         Arc<RelaySigner>,
    /// Configurable so tests/dev can point at a local relay. Empty string
    /// disables push entirely.
    relay_url:            String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse { (StatusCode::OK, "ok") }

async fn interrupt_handler(State(state): State<Arc<AppState>>) -> StatusCode {
    state.cancel.lock().unwrap().cancel();
    StatusCode::OK
}

async fn info_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "pubkey":               state.pubkey_b32,
        "relay_signing_pubkey": state.relay_signer.pubkey_b32(),
        "relay_url":            state.relay_url,
    }))
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

/// What kicked off this agentic turn.
enum TurnTrigger {
    /// A frame from mobile. Append the user message to the persisted history
    /// before running the turn.
    User(String),
    /// An autonomous follow-up triggered by a `bg_complete` row that already
    /// sits in `state.messages`. Don't append anything; just run a turn so
    /// the model sees the new tail and decides what to do.
    Auto,
}

/// Spawn an agentic turn. Returns immediately; events are buffered + fanned out
/// to all current /stream subscribers via `state.stream_state`. The caller must
/// have already verified `is_streaming` was false and flipped it to true.
fn spawn_turn(state: Arc<AppState>, trigger: TurnTrigger) {
    tokio::spawn(async move {
        let api_key = match resolve_api_key() {
            Some(k) => k,
            None => {
                let json = serde_json::json!({"type":"error","message":"no API key configured"}).to_string();
                buffer_and_fanout(&state.stream_state, json);
                state.is_streaming.store(false, Ordering::Relaxed);
                return;
            }
        };
        let model = resolve_model();

        if let TurnTrigger::User(text) = &trigger {
            let mut msgs = state.messages.lock().unwrap();
            msgs.push(ApiMessage {
                role:    "user".to_string(),
                content: vec![ContentBlock::Text { text: text.clone() }],
            });
            save_messages(&msgs);
        }

        // Snapshot the history we're sending into the model. Save the length so
        // we can splice mid-turn arrivals (e.g. a `bg_complete` row that lands
        // while we're streaming) back onto the end after the turn — without it
        // the post-turn `*msgs = updated` overwrite would clobber them.
        let messages: Vec<ApiMessage> = state.messages.lock().unwrap().iter()
            .filter(|m| m.role != "interrupted")
            .cloned()
            .collect();
        let snapshot_len = messages.len();
        let system    = state.system.clone();
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

        let extra_tools = build_tools_with_mcp(&state.mcp_pool, &lair_extra_tools()).await;
        let executor    = chain_executor_with_mcp(state.mcp_pool.clone(), lair_extra_executor(Arc::clone(&state)));

        // Agent task: drives the model loop, terminates with Result/Interrupted/Error.
        tokio::spawn(async move {
            match send_message(messages, &system, &model, &api_key, "/", Some(event_tx), cancel.clone(), &extra_tools, executor).await {
                Ok((_, cost_usd, mut updated)) => {
                    if cancel.is_cancelled() {
                        updated.push(ApiMessage {
                            role:    "interrupted".to_string(),
                            content: vec![ContentBlock::Text { text: "interrupted".to_string() }],
                        });
                        commit_turn(&msgs_arc, snapshot_len, updated);
                        *state_arc.last_cost_usd.lock().unwrap() = Some(cost_usd);
                        done_tx.send(ChatEvent::Interrupted { cost_usd }).await.ok();
                    } else {
                        commit_turn(&msgs_arc, snapshot_len, updated);
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
                    commit_turn(&msgs_arc, snapshot_len, partial);
                    done_tx.send(ChatEvent::Error { message: e }).await.ok();
                }
            }
        });

        // Relay task: drains the per-turn mpsc, buffers JSON, and fans it out
        // to every live /stream WS subscriber.
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
        info!("[lair/stream] turn complete, is_streaming=false");
        // If a background task completed mid-turn (or the just-finished turn
        // *was* an auto-turn that didn't fully drain pending bg_complete rows),
        // chain another agentic turn so the model gets to act on the result
        // without waiting for the next user message.
        try_continue_auto(state.clone());
    });
}

/// Splice the model's turn output back into `state.messages`, preserving any
/// rows that arrived during the turn (typically `bg_complete` from a
/// background-task completion). The model only saw `messages[..snapshot_len]`,
/// so its `updated` is `that_prefix + new_rows`. The persisted history was
/// `that_prefix + extras_since_snapshot`. Final shape:
///
///   updated_prefix + model_delta + extras_since_snapshot
///
/// (Extras go *after* the model's response so they sit at the tail and
/// `try_continue_auto` can pick them up as the next pending input.)
fn commit_turn(msgs_arc: &Arc<Mutex<Vec<ApiMessage>>>, snapshot_len: usize, updated: Vec<ApiMessage>) {
    let mut current = msgs_arc.lock().unwrap();
    let extras: Vec<ApiMessage> = if current.len() > snapshot_len {
        current.split_off(snapshot_len)
    } else {
        Vec::new()
    };
    *current = updated;
    current.extend(extras);
    save_messages(&current);
}

/// Atomically check whether the persisted history's tail is an unprocessed
/// `bg_complete` row, and if so, kick off an auto-turn so the model reacts.
/// Used both right after appending a `bg_complete` (in case no turn is in
/// flight) and at the end of every turn (in case a `bg_complete` arrived
/// mid-turn). Idempotent — losing the compare_exchange race means the other
/// caller will run the turn instead.
fn try_continue_auto(state: Arc<AppState>) {
    let needs_turn = matches!(
        state.messages.lock().unwrap().last().map(|m| m.role.as_str()),
        Some("bg_complete")
    );
    if !needs_turn { return; }
    if state.is_streaming
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return; // a turn is already running; it'll re-check on completion
    }
    info!("[lair/stream] auto-turn triggered by bg_complete");
    spawn_turn(state, TurnTrigger::Auto);
}

async fn handle_stream(socket: WebSocket, state: Arc<AppState>) {
    info!("[lair/stream] WebSocket connection opened");
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Hold off on greeting the client until subsystem init completes (first
    // K8s poll, or 30s soft cap). Without this, mobile shows "live" the moment
    // the WS opens against a freshly-restarted pod whose containers list and
    // MCP pool may still be settling — and a `user_message` sent in that
    // window can fail with a confusing 404.
    let mut ready_rx = state.ready_rx.clone();
    while !*ready_rx.borrow() {
        if ready_rx.changed().await.is_err() { break; }
    }

    // Atomically snapshot the per-turn buffer (events from any in-flight turn)
    // and register as a subscriber so no events are lost in the gap. The buffer
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
    // Send an initial containers snapshot so the UI can render immediately.
    {
        let snapshot = state.containers_rx.borrow().clone();
        let json = serde_json::json!({"type":"containers","containers":snapshot}).to_string();
        if ws_tx.send(WsMessage::Text(json)).await.is_err() {
            return;
        }
    }
    // One-shot tasks snapshot so the modal renders without waiting for a change.
    if ws_tx.send(WsMessage::Text(tasks_wire_json(&state.stream_state))).await.is_err() {
        return;
    }
    if !replay.is_empty() {
        info!("[lair/stream] replaying {} buffered event(s) to new connection", replay.len());
        for event in replay {
            if ws_tx.send(WsMessage::Text(event)).await.is_err() { return; }
        }
    }

    let mut containers_rx = state.containers_rx.clone();

    // App-level keepalive: server emits `ping` every KEEPALIVE_INTERVAL; client
    // must echo `pong` with the same id. After KEEPALIVE_MAX_MISSED unacked
    // pings we evict the WS so half-open connections (NAT timeout, dead peer)
    // don't leak the task forever.
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

            // Outgoing: container list updates from the K8s poller.
            res = containers_rx.changed() => {
                if res.is_err() { break; }
                let list = containers_rx.borrow_and_update().clone();
                let json = serde_json::json!({"type":"containers","containers":list}).to_string();
                if ws_tx.send(WsMessage::Text(json)).await.is_err() { break; }
            },

            // Outgoing: keepalive ping. Evict if too many outstanding.
            _ = ping_interval.tick() => {
                let outstanding = next_ping_id.saturating_sub(last_acked_id);
                if outstanding >= KEEPALIVE_MAX_MISSED {
                    warn!("[lair/stream] evicting peer: {outstanding} unacked ping(s)");
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

    info!("[lair/stream] connection closed");
}

/// Dispatch a client → server frame parsed from a /stream WS message.
async fn handle_client_frame(raw: &str, state: &Arc<AppState>) {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v)  => v,
        Err(_) => {
            warn!("[lair/stream] dropping unparseable client frame");
            return;
        }
    };
    let frame_type = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
    match frame_type {
        "user_message" => {
            let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if text.is_empty() {
                warn!("[lair/stream] user_message frame missing/empty text");
                return;
            }
            // Reject overlapping turns. Mobile gates sends on its own
            // status, but a buggy or malicious client could try anyway.
            if state.is_streaming
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                let json = serde_json::json!({"type":"error","message":"a turn is already running"}).to_string();
                buffer_and_fanout(&state.stream_state, json);
                return;
            }
            let preview: String = text.chars().take(120).collect();
            info!("[lair/stream] user_message ({} chars): {preview}", text.len());
            spawn_turn(state.clone(), TurnTrigger::User(text));
        }
        "interrupt" => {
            info!("[lair/stream] interrupt frame received");
            state.cancel.lock().unwrap().cancel();
            // Optimistic ack — the agentic loop will follow up with Interrupted.
            buffer_and_fanout(&state.stream_state, serde_json::json!({"type":"interrupt_ack"}).to_string());
        }
        "start_container" => {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if id.is_empty() {
                warn!("[lair/stream] start_container frame missing id");
                return;
            }
            info!("[lair/stream] start_container id={id}");
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(e) = start_container_by_id(&state, &id).await {
                    error!("[lair/stream] start_container failed: {e}");
                    let json = serde_json::json!({"type":"error","message":format!("start_container: {e}")}).to_string();
                    buffer_and_fanout(&state.stream_state, json);
                }
            });
        }
        "terminate_agent" => {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if id.is_empty() {
                warn!("[lair/stream] terminate_agent frame missing id");
                return;
            }
            info!("[lair/stream] terminate_agent id={id}");
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(e) = terminate_agent_by_id(&state, &id).await {
                    error!("[lair/stream] terminate_agent failed: {e}");
                    let json = serde_json::json!({"type":"error","message":format!("terminate_agent: {e}")}).to_string();
                    buffer_and_fanout(&state.stream_state, json);
                }
            });
        }
        "cancel_task" => {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if id.is_empty() {
                warn!("[lair/stream] cancel_task frame missing id");
                return;
            }
            let fired = core_cancel_task(&state.stream_state, &id);
            info!("[lair/stream] cancel_task id={id} fired={fired}");
            // No ack frame — the spawn's deliver closure will push a tasks
            // snapshot once the inner agentic loop honours the cancellation.
        }
        "pong" => {
            // App-level keepalive ack — handled per-WS in the future ping/pong work.
            // For now just no-op so unknown clients don't see it as an error.
        }
        other => {
            warn!("[lair/stream] unknown client frame type='{other}'");
        }
    }
}

async fn clear_handler(State(state): State<Arc<AppState>>) -> StatusCode {
    info!("[lair/clear] clearing conversation history");
    let mut msgs = state.messages.lock().unwrap();
    msgs.clear();
    save_messages(&msgs);
    StatusCode::OK
}

/// Resume a stopped agent container. Backs the `start_container` /stream
/// frame mobile sends when the user taps a non-running child.
async fn start_container_by_id(state: &AppState, id: &str) -> Result<(), String> {
    let name = state
        .containers_rx
        .borrow()
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.name.clone())
        .ok_or_else(|| format!("container '{id}' not found"))?;

    docker_ops::start_container(&state.docker, &name)
        .await
        .map_err(|e| e.to_string())?;
    {
        let mut reg = state.registry.lock().unwrap();
        let _ = reg.update_status(&name, AgentStatus::Running);
    }
    info!("[containers] started {name}, triggering re-poll");
    state.poll_trigger.notify_one();
    Ok(())
}

/// Remove the named agent container and both of its named volumes. Backs the
/// `terminate_agent` /stream frame so the mobile UI can long-press to terminate.
async fn terminate_agent_by_id(state: &AppState, id: &str) -> Result<(), String> {
    let name = state
        .containers_rx
        .borrow()
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.name.clone())
        .ok_or_else(|| format!("agent '{id}' not found"))?;

    docker_ops::destroy_container(&state.docker, &name, /*remove_volumes=*/true)
        .await
        .map_err(|e| e.to_string())?;
    {
        let mut reg = state.registry.lock().unwrap();
        let _ = reg.remove(&name);
    }
    info!("[containers] terminated {name}, triggering re-poll");
    state.poll_trigger.notify_one();
    Ok(())
}

// ── Container poller ──────────────────────────────────────────────────────────

/// Reconcile the registry against Docker every 10s (and on demand via
/// `poll_trigger`). Docker is authoritative for runtime status; the registry
/// is authoritative for identity (name, port, pubkey, git_url, …). Anything in
/// Docker that the registry doesn't know about is ignored (e.g. an admin
/// `docker run` outside lair's control); anything in the registry that's
/// missing from Docker is treated as `Stopped` and the in-memory copy is
/// updated so mobile sees the dead row.
async fn poll_containers(state: Arc<AppState>, ready_tx: watch::Sender<bool>) {
    info!("[containers] poller starting, initial delay 5s");
    tokio::time::sleep(Duration::from_secs(5)).await;
    let mut first_iter = true;
    loop {
        debug!("[containers] polling Docker for managed containers");
        match docker_ops::list_managed(&state.docker).await {
            Ok(docker_list) => {
                debug!("[containers] Docker returned {} container(s)", docker_list.len());
                let live: std::collections::HashMap<String, String> = docker_list
                    .iter()
                    .map(|c| (c.name.clone(), c.state.clone()))
                    .collect();

                let new_containers: Vec<ContainerInfo> = {
                    let mut reg = state.registry.lock().unwrap();
                    let now = octo_core::now_secs();
                    let snapshot = reg.list().to_vec();
                    let mut out = Vec::with_capacity(snapshot.len());
                    for record in snapshot {
                        // Remote agents aren't managed by the local Docker
                        // daemon — they live on whatever VM `register_remote_agent`
                        // pointed lair at. Surface them as-is; the LLM /
                        // operator is responsible for marking them
                        // stopped (via `forget_agent`).
                        if record.is_remote() {
                            out.push(ContainerInfo {
                                id:      record.name.clone(),
                                name:    record.name.clone(),
                                git_url: record.git_url.clone().unwrap_or_default(),
                                status:  record.status.as_wire_str().to_string(),
                                host:    record.host.clone().unwrap_or_else(|| state.public_host.clone()),
                                port:    record.port,
                                pubkey:  record.pubkey.clone(),
                            });
                            continue;
                        }
                        match live.get(&record.name) {
                            Some(state_str) => {
                                let status = status_from_docker(state_str);
                                let _ = reg.update_status(&record.name, status);
                                let _ = reg.update_last_seen(&record.name, now);
                                out.push(ContainerInfo {
                                    id:      record.name.clone(),
                                    name:    record.name.clone(),
                                    git_url: record.git_url.clone().unwrap_or_default(),
                                    status:  status.as_wire_str().to_string(),
                                    host:    record.host.clone().unwrap_or_else(|| state.public_host.clone()),
                                    port:    record.port,
                                    pubkey:  record.pubkey.clone(),
                                });
                            }
                            None => {
                                // Local container disappeared from Docker —
                                // assume it was removed out-of-band (e.g. by
                                // `octo agents delete`). Drop the registry row.
                                let _ = reg.remove(&record.name);
                                info!("[containers] dropped registry entry '{}' (container absent)", record.name);
                            }
                        }
                    }
                    out
                };

                let changed = *state.containers_tx.borrow() != new_containers;
                if changed {
                    let n = new_containers.len();
                    let names = new_containers.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ");
                    info!("[containers] state changed: {n} child(ren): {names}");
                    // send_replace ignores no-receiver errors; containers_rx in AppState
                    // also keeps the channel alive even with zero open WS connections.
                    state.containers_tx.send_replace(new_containers);
                }
            }
            Err(e) => error!("[containers] poll error: {e}"),
        }
        if first_iter {
            first_iter = false;
            ready_tx.send_replace(true);
            info!("[containers] first poll complete — server marked ready");
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {
                debug!("[containers] poll interval elapsed");
            }
            _ = state.poll_trigger.notified() => {
                info!("[containers] poll triggered manually");
            }
        }
    }
}

// ── System prompt ─────────────────────────────────────────────────────────────

fn build_system_prompt() -> String {
    r#"# Identity & context
You are "lair" — the control-plane agent of an octo deployment. You run as a process on a single host machine; sibling "child" agent containers run on the same host and you orchestrate them via the local Docker daemon. The user is talking to you over an encrypted Noise tunnel from a mobile or desktop client; you are usually the first agent they reach. From here they create, inspect, and tear down children (each a separate Docker container, typically pinned to one git repository). To talk *to* a child the user opens its own chat in the mobile app — you do not relay messages.

octo can host any kind of agent workload, not only coding agents — don't assume the user is doing software work unless they say so.

# What you help with
1. Orchestration — spin up, tear down, and inspect children.
2. Direct work — answer questions, run shell commands, read external resources, and handle small fixes that don't require a child's repo.

# Environment
- Docker host. Children are containers managed via the Docker API; each has its own pair of named volumes (`agent-<name>-data`, `agent-<name>-workspace`) that survive restarts.
- `gh` is installed and `GH_TOKEN` is set — no login step needed.
- Each child publishes its Noise port on a host port in the 30100–30199 range; mobile reaches the child directly via that port.
- MCP servers may be configured at init time or hot-added at runtime; their tools appear alongside the built-ins. `web_fetch` (and `web_search` when Brave is configured) cover external lookups.
- A path prefixed with `@` (e.g. `@core/src/lib.rs`) is a file reference inside a repo — treat it as a path.

# Orchestration tools (lair-specific)
- **`list_agents`** — all known agents (local + remote) with their full registry rows (status, host, port, pubkey, git_url, instance_id, provider, metadata). Cheap; call before guessing a name.
- **`create_agent`** — args: `git_url?`, `name?`, `noise_port?`, `startup_script?`, `startup_prompt?`. Creates a *local* Docker container on this host.
  - Omit `git_url` for a repo-less workload (default name `lair-workload`); otherwise default name is `lair-<repo-slug>`.
  - `noise_port` auto-assigns from 30100–30199 if omitted.
  - `startup_script` runs before the child's server boots — good for `apt-get`, package installs, git config.
  - `startup_prompt` is sent as the child's first user message once it's ready and triggers a full agentic loop.
  - **Both fields are stored as plaintext env vars on the container.** Never put API keys, tokens, or other secrets in them. If the user asks for that, push back and suggest a safer route (MCP env).
- **`mint_bootstrap_userdata`** — args: `name`, `agent_purpose?`, `startup_script?`, `public_port?`. Returns a cloud-init bash script for a **remote** agent. The userdata is **credentials-free**: it trusts lair's SSH pubkey, installs Docker + git, and starts the agent container in a minimal mode (no API keys, no git_url). Hand the returned `userdata` to whichever provisioning MCP they have configured (AWS, Hetzner, etc.). The MCP will return the new VM's IP — then call `register_remote_agent`, which finishes the bootstrap over SSH.
- **`register_remote_agent`** — args: `name`, `host`, `provider?`, `instance_id?`, `git_url?`, `metadata?`. Call this after the provisioning MCP returns the new VM's IP. Lair SSHes in (using its operator key) and: (a) waits for the agent container to publish `/var/lib/octo/agent-data/agent-info.json`, (b) drops `config.json` with the API keys lair has in its own env, (c) clones `git_url` into the workspace if given (using lair's `GH_TOKEN` — never sent through the cloud provider), (d) `docker restart`s the agent so it picks everything up. Total timeout ~6 minutes. `name` must match what you passed to `mint_bootstrap_userdata`.
- **`terminate_agent(name)`** — *destructive, local agents only.* Removes the container and both named volumes (`agent-<name>-data`, `agent-<name>-workspace`). For remote agents, returns instructions to terminate the VM via the provisioning MCP first, then call `forget_agent`. Always run `list_agents` first to confirm the exact name; confirm with the user before calling unless the request was unambiguous.
- **`forget_agent(name)`** — *registry-only.* Removes the row without touching Docker or the VM. Used after terminating a remote instance via a provisioning MCP. Don't use on a live local agent; use `terminate_agent` instead.
- **`restart_all_containers`** — restart every local agent container. Use after pulling a new image; not for routine flakes. Has no effect on remote agents.
- **`run_background_task(task_description)`** — spawn a long-running task in the background and return immediately. The user is notified when it finishes. Use for work that would otherwise block the current turn for minutes (long builds, multi-step research, repo-wide refactors). The task description must be self-contained — the background loop does not inherit conversation history.
  - When a background task completes, the result is injected into this conversation as a "Background task … completed" message and you'll be invoked autonomously to react. **If no follow-up action is genuinely useful, reply with one short line acknowledging the result** (e.g. "Background task done — no further action needed.") rather than producing prose. Only continue working if the result clearly demands it (a reported failure to investigate, a ready artefact the user would want to use next, etc).

# General tools (shared with children)
- `bash` — shell commands; use for git, gh, curl, docker (read-only diagnostics only — never mutate), one-offs.
- `read_file(path, offset?, limit?)` — pair with `grep` first; never read a whole file just to skim.
- `grep(pattern, path?, context?)` — returns `file:line` you can feed back into `read_file`.
- `glob(pattern)` — file-path search. Anchor from a known root; never start a path argument with `**`.
- `edit_file(path, old_str, new_str)` — exact string replace; `old_str` must match exactly once. Prefer over `write_file` on existing files.
- `write_file(path, content)` — new files only.

# Working with children
- You orchestrate children (create / inspect / terminate); you do **not** message them. If the user asks "have child X do Y", tell them to open the child's own chat in the mobile app — that's the direct path. You can still answer cluster-wide questions about the child (status, port, git_url) from `list_agents`.
- Don't try to `docker exec` into a child to do its work for it. Direct work in a child's repo belongs to that child's own chat.

# Local vs remote agents
- **Local**: `create_agent` → Docker container on this host. Reachable on `host:30100–30199`. Default for "spin up an agent" unless the user specifies a cloud / instance type.
- **Remote**: a 3-step LLM-driven flow that uses the user's configured cloud MCP. **Userdata carries no credentials** — lair finishes the bootstrap over SSH after the VM is up, so the cloud provider and the provisioning MCP never see API keys.
  - **NEVER hand-write the userdata yourself.** The lair Docker image's default ENTRYPOINT runs `--role lair`, and only the userdata blob produced by `mint_bootstrap_userdata` flips it to `--role agent` (by appending `/usr/local/bin/octo-lair --role agent` to its `docker run` line) and trusts your SSH pubkey. If you skip the tool and write your own userdata, you'll silently boot a second *lair* on the VM instead of an agent, `register_remote_agent` will time out waiting for an `agent-info.json` that the wrong role never produces, and SSH auth will fail because no one put your key in `authorized_keys`. Always call `mint_bootstrap_userdata` and pass its `userdata` field verbatim to the provisioning MCP.
  1. `mint_bootstrap_userdata(name=…, agent_purpose?=…, …)` — get the credentials-free userdata blob. Use exactly the `userdata` string it returns; do not edit it.
  2. Call the provisioning MCP (`aws_run_instances`, `hetzner_create_server`, etc.) with that userdata (passed verbatim), plus whatever the user specified (instance type, region, security group with TCP 22 + 9000 inbound from this host). The MCP returns a public IP and instance id.
  3. `register_remote_agent(name=…, host=<public_ip>, git_url?=…, provider=…, instance_id=…)` — lair SSHes in, waits for the agent to publish its identity, drops `config.json` with the API keys, clones `git_url` if given (using lair's GH_TOKEN), restarts the agent, registers. The `name` must match step 1; pass `git_url` here (not to `mint_bootstrap_userdata`).
- Your SSH keypair lives at `/data/ssh_id_ed25519`. `mint_bootstrap_userdata` **always** embeds the matching pubkey in the userdata it returns, so any VM provisioned with that userdata trusts your key on first boot — `ssh -i /data/ssh_id_ed25519 root@<host>` works against any remote agent you've provisioned, no extra setup.
- Termination mirrors creation. For remote agents `terminate_agent` will *not* clean up the VM — call the provisioning MCP's terminate-instance method first (using `instance_id` from `list_agents`), then `forget_agent(name)`.
- Trigger the remote flow when the user names a cloud / instance type / region, OR when they ask for hardware lair doesn't have locally (GPUs, etc.).

# Response style
- Concise and direct; the user is often on a phone screen.
- Don't narrate tool calls ("Let me check…", "I'll now…", "I've completed…").
- Don't summarize tool output back to the user — they can see it. Write prose only for real answers, questions, or recommendations.
- No filler openers ("Sure!", "Of course!", "Great question!").
- When you call a tool, call it — don't announce it first.

# Safety
- Never commit or push git changes unless the user explicitly asked.
- Confirm before `terminate_agent` or `restart_all_containers` unless the user just told you to.
- If a request would put a secret into plaintext container config (`startup_script`, `startup_prompt`, env), flag it and offer a safer alternative.
- Trust your judgment on small choices; only ask when ambiguity would actually change the outcome."#
        .to_string()
}

// ── Tools ─────────────────────────────────────────────────────────────────────

fn create_agent_tool() -> AnthropicTool {
    AnthropicTool {
        name: "create_agent".to_string(),
        description: "Create and start a new octo child agent as a Docker container on the lair host. \
                       Handles host-port assignment (30100–30199), per-agent named volumes, and the container itself."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "git_url": {
                    "type": "string",
                    "description": "The Git repository URL to clone and operate on. Omit to start a container without a repository (e.g. for ML workloads or arbitrary compute)."
                },
                "name": {
                    "type": "string",
                    "description": "Optional name override. Defaults to lair-<repo-name>, or lair-workload if no git_url."
                },
                "noise_port": {
                    "type": "integer",
                    "description": "Optional host port for Noise traffic (30100–30199). Auto-assigned if omitted."
                },
                "startup_script": {
                    "type": "string",
                    "description": "Optional shell script run inside the child before the server starts. Never include sensitive data such as API keys or tokens — these are stored as plaintext env vars on the container."
                },
                "startup_prompt": {
                    "type": "string",
                    "description": "Optional initial prompt sent to the child's agentic loop once ready. Never include sensitive data such as API keys or tokens — these are stored as plaintext env vars on the container."
                }
            },
            "required": []
        }),
        display_label: Some("Creating agent".into()),
    }
}

fn terminate_agent_tool() -> AnthropicTool {
    AnthropicTool {
        name: "terminate_agent".to_string(),
        description: "Permanently terminate a child agent: remove its Docker container and \
                       delete both named volumes. Irreversible — all data in /data and /workspace is lost."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The name of the child to terminate."
                }
            },
            "required": ["name"]
        }),
        display_label: Some("Terminating agent".into()),
    }
}

fn list_agents_tool() -> AnthropicTool {
    AnthropicTool {
        name: "list_agents".to_string(),
        description: "List every known agent — local and remote — with the full registry row \
                       (status, host, port, pubkey, git_url, instance_id, provider, metadata). \
                       Cheap; call before guessing a name."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
        display_label: Some("Listing agents".into()),
    }
}

fn mint_bootstrap_userdata_tool() -> AnthropicTool {
    AnthropicTool {
        name: "mint_bootstrap_userdata".to_string(),
        description: "Mint a cloud-init bash script (\"user data\") for bootstrapping a remote \
                       octo agent on a freshly-provisioned VM. The userdata contains **no \
                       credentials** — only lair's SSH public key, a Docker install, and a \
                       `docker run` that starts the agent in a minimal mode. After the MCP \
                       returns the VM's IP, call `register_remote_agent`; lair will SSH in and \
                       finish bootstrapping (drop the config.json with API keys, clone the repo \
                       if `git_url` was given, restart the agent so it picks everything up). \
                       Returns the userdata blob plus the agent name."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Logical name for the new agent; reused in register_remote_agent."
                },
                "agent_purpose": {
                    "type": "string",
                    "description": "One-line mission baked into the agent's system prompt (used only if no git_url is later supplied)."
                },
                "startup_script": {
                    "type": "string",
                    "description": "Optional bash run inside the agent container at boot, before lair finishes the bootstrap. Will not have access to API keys (they arrive later via SSH); use it for package installs and similar."
                },
                "public_port": {
                    "type": "integer",
                    "description": "Host port on the VM that publishes the agent's Noise endpoint (default 9000). Security group must allow inbound TCP on this port plus SSH (22) from lair's IP."
                }
            },
            "required": ["name"]
        }),
        display_label: Some("Minting userdata".into()),
    }
}

fn register_remote_agent_tool() -> AnthropicTool {
    AnthropicTool {
        name: "register_remote_agent".to_string(),
        description: "Finish bootstrapping a remote agent and register it with lair. SSHes in \
                       (using lair's operator key), waits for the agent container to publish \
                       `/var/lib/octo/agent-data/agent-info.json`, drops a `config.json` with the \
                       API keys, optionally clones `git_url` into the workspace, and `docker \
                       restart`s the agent so it picks everything up. Total timeout ~6 minutes \
                       (cloud-init can take a few). `name` must match what you passed to \
                       `mint_bootstrap_userdata`. Each SSH op retries internally with exponential \
                       backoff to absorb sshd-during-cloud-init flakes. A `Pending` registry row \
                       is inserted as soon as the agent's identity is known, so the row is \
                       visible to mobile (and to `list_agents`) while the rest of the bootstrap \
                       runs. On a hard SSH error mid-flow the row stays Pending — re-call the \
                       tool with the same `name` and `host` to resume (all phases are idempotent)."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Logical agent name — must match mint_bootstrap_userdata."
                },
                "host": {
                    "type": "string",
                    "description": "Public IP or DNS name of the VM (from the provisioning MCP's response)."
                },
                "provider": {
                    "type": "string",
                    "description": "Free-form provider tag (e.g. aws, gcp, hetzner). Used by future terminate flows."
                },
                "instance_id": {
                    "type": "string",
                    "description": "Cloud instance id (e.g. i-0abc...). Stored verbatim for later terminate calls."
                },
                "git_url": {
                    "type": "string",
                    "description": "Optional Git URL to clone into the agent's workspace after the container is up. Lair uses its own GH_TOKEN for HTTPS clones — the URL itself stays out of the cloud-provider's view."
                },
                "metadata": {
                    "type": "object",
                    "description": "Opaque provider-specific blob (region, instance_type, image id, ...). Stored alongside the row."
                }
            },
            "required": ["name", "host"]
        }),
        display_label: Some("Registering remote agent".into()),
    }
}

fn forget_agent_tool() -> AnthropicTool {
    AnthropicTool {
        name: "forget_agent".to_string(),
        description: "Remove an agent's registry row without touching Docker or any VM. Use this \
                       after the provisioning MCP has terminated a remote instance, to clean up the \
                       dangling row. Don't use on a live local agent — use `terminate_agent` instead."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Agent name to forget."
                }
            },
            "required": ["name"]
        }),
        display_label: Some("Forgetting agent".into()),
    }
}

fn restart_all_containers_tool() -> AnthropicTool {
    AnthropicTool {
        name: "restart_all_containers".to_string(),
        description: "Restart every managed agent container so they pick up new state. \
                       Use this after pulling a new image to apply the update; not for routine flakes."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
        display_label: Some("Restarting containers".into()),
    }
}

fn lair_extra_tools() -> Vec<AnthropicTool> {
    vec![
        list_agents_tool(),
        create_agent_tool(),
        mint_bootstrap_userdata_tool(),
        register_remote_agent_tool(),
        terminate_agent_tool(),
        forget_agent_tool(),
        restart_all_containers_tool(),
        run_background_task_tool(),
    ]
}

fn lair_extra_executor(state: Arc<AppState>) -> Option<Arc<dyn Fn(String, serde_json::Value)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
    + Send + Sync>>
{
    Some(Arc::new(move |name: String, input: serde_json::Value| {
        let state  = state.clone();
        Box::pin(async move {
            match name.as_str() {
                "list_agents"              => exec_list_agents(state.clone()).await,
                "create_agent"             => exec_create_agent(state, input).await,
                "mint_bootstrap_userdata"  => exec_mint_bootstrap_userdata(state, input).await,
                "register_remote_agent"    => exec_register_remote_agent(state, input).await,
                "terminate_agent"          => exec_terminate_agent(state, input).await,
                "forget_agent"             => exec_forget_agent(state, input).await,
                "restart_all_containers"   => exec_restart_all_containers(state).await,
                "run_background_task"      => exec_run_background_task(state, input).await,
                other => format!("unknown tool: {other}"),
            }
        })
    }))
}

async fn exec_list_agents(state: Arc<AppState>) -> String {
    // Surface full registry rows so the LLM has access to instance_id, provider,
    // and metadata when working with remote agents.
    let records = state.registry.lock().unwrap().list().to_vec();
    serde_json::to_string_pretty(&records).unwrap_or_else(|e| format!("error: {e}"))
}

async fn exec_create_agent(state: Arc<AppState>, input: serde_json::Value) -> String {
    let git_url = input.get("git_url").and_then(|v| v.as_str()).map(str::to_string);

    let child_name = input.get("name").and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            match &git_url {
                Some(u) => {
                    let slug = u.trim_end_matches('/')
                        .split('/')
                        .last()
                        .unwrap_or("repo")
                        .trim_end_matches(".git")
                        .to_lowercase();
                    format!("lair-{slug}")
                }
                None => format!("lair-workload"),
            }
        });

    let pub_host          = state.public_host.clone();
    let noise_private_key = state.noise_private_key_hex.clone();
    let startup_script = input.get("startup_script").and_then(|v| v.as_str()).map(str::to_string);
    let startup_prompt = input.get("startup_prompt").and_then(|v| v.as_str()).map(str::to_string);

    // Resolve host port — explicit override or first free slot in the
    // conventional 30100–30199 range, scoped to the registry.
    let noise_port: u16 = match input.get("noise_port").and_then(|v| v.as_u64()) {
        Some(p) => p as u16,
        None => match state.registry.lock().unwrap().assign_free_port(30100..=30199) {
            Some(p) => p,
            None    => return "error: no free host ports in 30100–30199".to_string(),
        },
    };

    info!("[lair/create_agent] creating {child_name} port={noise_port} git={}", git_url.as_deref().unwrap_or("(none)"));

    // Inherit provider credentials from lair's own env so children can run
    // their loops without a separate secret store. Mirrors what the old
    // `child-secrets` envFrom did, just sourced from the parent process.
    let anthropic_api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let gh_token          = std::env::var("GH_TOKEN").ok();
    let model             = std::env::var("MODEL").ok();
    let openai_api_url    = std::env::var("OPENAI_API_URL").ok();
    let openai_api_key    = std::env::var("OPENAI_API_KEY").ok();

    let image = std::env::var("OCTO_AGENT_IMAGE")
        .unwrap_or_else(|_| docker_ops::DEFAULT_AGENT_IMAGE.to_string());

    let params = docker_ops::CreateAgentParams {
        name:              &child_name,
        image:             &image,
        git_url:           git_url.as_deref(),
        host_noise_port:   noise_port,
        public_host:       &pub_host,
        noise_private_key: &noise_private_key,
        startup_script:    startup_script.as_deref(),
        startup_prompt:    startup_prompt.as_deref(),
        anthropic_api_key: anthropic_api_key.as_deref(),
        gh_token:          gh_token.as_deref(),
        model:             model.as_deref(),
        openai_api_url:    openai_api_url.as_deref(),
        openai_api_key:    openai_api_key.as_deref(),
        agent_purpose:     None,
    };

    match docker_ops::create_agent_container(&state.docker, &params).await {
        Ok(container_id) => {
            let now = octo_core::now_secs();
            let record = AgentRecord {
                name:          child_name.clone(),
                container_id:  Some(container_id),
                host:          None,
                port:          noise_port,
                pubkey:        state.pubkey_b32.clone(),
                git_url:       git_url.clone(),
                status:        AgentStatus::Pending,
                image_version: image.clone(),
                created_at:    now,
                last_seen:     now,
                instance_id:   None,
                provider:      None,
                metadata:      serde_json::Value::Null,
            };
            if let Err(e) = state.registry.lock().unwrap().add(record) {
                error!("[lair/create_agent] registry add failed: {e:#}");
                return format!("error registering '{child_name}': {e:#}");
            }
            info!("[lair/create_agent] created {child_name}");
            state.poll_trigger.notify_one();
            format!("Created child '{child_name}' on host port {noise_port}.")
        }
        Err(e) => {
            error!("[lair/create_agent] failed: {e:#}");
            format!("error: {e:#}")
        }
    }
}

async fn exec_terminate_agent(state: Arc<AppState>, input: serde_json::Value) -> String {
    let name = match input.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return "error: missing 'name' field".to_string(),
    };

    let record = state.registry.lock().unwrap().get(&name).cloned();
    let record = match record {
        Some(r) => r,
        None    => return format!("error: no agent named '{name}' in the registry"),
    };

    if record.is_remote() {
        let provider    = record.provider.as_deref().unwrap_or("(unknown provider)");
        let instance_id = record.instance_id.as_deref().unwrap_or("(no instance_id)");
        return format!(
            "'{name}' is a remote agent (provider={provider}, instance_id={instance_id}).\n\
             `terminate_agent` only destroys local Docker containers. To tear this down:\n\
             1. Use the {provider} provisioning MCP to terminate instance {instance_id}.\n\
             2. Call `forget_agent` with name='{name}' to remove the registry row.\n\
             metadata for the MCP call: {}",
            record.metadata,
        );
    }

    info!("[lair/terminate_agent] terminating local '{name}'");
    match docker_ops::destroy_container(&state.docker, &name, /*remove_volumes=*/true).await {
        Ok(_) => {
            {
                let mut reg = state.registry.lock().unwrap();
                let _ = reg.remove(&name);
            }
            info!("[lair/terminate_agent] '{name}' deleted, triggering re-poll");
            state.poll_trigger.notify_one();
            format!("Terminated '{name}' and deleted both named volumes.")
        }
        Err(e) => {
            error!("[lair/terminate_agent] failed to delete '{name}': {e:#}");
            format!("error: {e:#}")
        }
    }
}

async fn exec_mint_bootstrap_userdata(state: Arc<AppState>, input: serde_json::Value) -> String {
    let name = match input.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return "error: missing 'name' field".to_string(),
    };
    if state.registry.lock().unwrap().get(&name).is_some() {
        return format!("error: agent '{name}' already exists in the registry");
    }

    let agent_purpose  = input.get("agent_purpose") .and_then(|v| v.as_str()).map(str::to_string);
    let startup_script = input.get("startup_script").and_then(|v| v.as_str()).map(str::to_string);
    let public_port    = input.get("public_port")   .and_then(|v| v.as_u64()).unwrap_or(9000) as u16;

    let lair_pubkey = match ssh_ops::read_lair_public_key() {
        Ok(k) => k,
        Err(e) => return format!("error reading lair SSH public key: {e:#}"),
    };

    let image = std::env::var("OCTO_AGENT_IMAGE")
        .unwrap_or_else(|_| docker_ops::DEFAULT_AGENT_IMAGE.to_string());

    // Userdata env carries no credentials and no git URL — only the
    // non-secret bootstrap config that the agent needs at process start.
    // API keys, the git clone, and `docker restart` all happen later, over
    // the SSH connection lair opens during `register_remote_agent`.
    let mut env_lines: Vec<String> = vec![
        "NOISE_PORT=9000".to_string(),
        format!("PUBLIC_PORT={public_port}"),
        "OCTO_DATA_DIR=/data".to_string(),
        "NOISE_KEY_FILE=/data/noise_key.bin".to_string(),
        "OCTO_SKIP_SHELL_ENV=1".to_string(),
    ];
    if let Some(v) = &agent_purpose  { env_lines.push(format!("AGENT_PURPOSE={v}")); }
    if let Some(v) = &startup_script { env_lines.push(format!("STARTUP_SCRIPT={v}")); }
    let env_content = env_lines.join("\n");

    let userdata = format!(r#"#!/bin/bash
set -eux

# 1. Trust lair's operator SSH key so lair can finish bootstrapping over SSH.
mkdir -p /root/.ssh
chmod 700 /root/.ssh
cat >> /root/.ssh/authorized_keys <<'OCTO_SSH_PUBKEY_EOF'
{lair_pubkey}
OCTO_SSH_PUBKEY_EOF
chmod 600 /root/.ssh/authorized_keys

# 2. Install Docker + git (git is for the SSH-driven clone lair runs later).
if ! command -v docker >/dev/null 2>&1; then
    curl -fsSL https://get.docker.com | sh
fi
if ! command -v git >/dev/null 2>&1; then
    if command -v apt-get >/dev/null 2>&1; then
        apt-get update && apt-get install -y git
    elif command -v yum >/dev/null 2>&1; then
        yum install -y git
    elif command -v apk >/dev/null 2>&1; then
        apk add --no-cache git
    fi
fi

# 3. Prepare bind-mounted dirs the agent container will see as /data and /workspace.
mkdir -p /var/lib/octo /var/lib/octo/agent-data /var/lib/octo/agent-workspace

# 4. Env file with the agent's non-secret bootstrap config.
umask 077
cat > /var/lib/octo/agent.env <<'OCTO_ENV_EOF'
{env_content}
OCTO_ENV_EOF
umask 022

# 5. Pull and run the agent. It boots without API keys; lair will drop
#    /data/config.json over SSH and `docker restart` this container to apply.
docker pull {image}
docker rm -f octo-agent 2>/dev/null || true
docker run -d \
    --name octo-agent \
    --restart unless-stopped \
    --label octo.managed=1 \
    --label octo.role=agent \
    -p {public_port}:9000 \
    -v /var/lib/octo/agent-data:/data \
    -v /var/lib/octo/agent-workspace:/workspace \
    --env-file /var/lib/octo/agent.env \
    {image} /usr/local/bin/octo-lair --role agent
"#);

    let result = serde_json::json!({
        "name":     name,
        "image":    image,
        "userdata": userdata,
        "instructions": format!(
            "Hand `userdata` to the provisioning MCP as the new instance's user-data. \
             Make sure the security group / firewall allows inbound TCP {public_port} (for mobile) \
             and 22 (for lair's SSH-driven bootstrap). The userdata contains no credentials. \
             After the MCP returns the public IP, call \
             register_remote_agent(name='{name}', host=<public_ip>, ...).",
        ),
    });
    serde_json::to_string_pretty(&result).unwrap_or_else(|e| format!("error: {e}"))
}

async fn exec_register_remote_agent(state: Arc<AppState>, input: serde_json::Value) -> String {
    let name = match input.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return "error: missing 'name' field".to_string(),
    };
    let host = match input.get("host").and_then(|v| v.as_str()) {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => return "error: missing 'host' field".to_string(),
    };
    let provider    = input.get("provider")   .and_then(|v| v.as_str()).map(str::to_string);
    let instance_id = input.get("instance_id").and_then(|v| v.as_str()).map(str::to_string);
    let git_url     = input.get("git_url")    .and_then(|v| v.as_str()).map(str::to_string);
    let metadata    = input.get("metadata")   .cloned().unwrap_or(serde_json::Value::Null);

    let key_path = octo_core::data_dir().join(octo_core::SSH_PRIVATE_KEY_FILE);
    if !key_path.exists() {
        return format!(
            "error: lair has no SSH private key at {}. Run `octo init` (or restart lair) to generate one.",
            key_path.display(),
        );
    }

    // Resumption logic. Three cases:
    // 1. No prior row → fresh registration.
    // 2. Prior row is Pending on the same host → resume (re-run all phases;
    //    each is idempotent, finished work is a no-op).
    // 3. Anything else (Running, or Pending on a different host) → refuse.
    let prior = state.registry.lock().unwrap().get(&name).cloned();
    let (created_at, resuming) = match prior {
        Some(r) if matches!(r.status, AgentStatus::Pending)
                && r.host.as_deref() == Some(host.as_str()) => {
            info!("[lair/register_remote_agent] resuming pending registration of '{name}' at {host}");
            (r.created_at, true)
        }
        Some(r) => {
            return format!(
                "error: agent '{name}' is already in the registry (status={}, host={:?}). \
                 If you need to re-register, call `forget_agent` first.",
                r.status.as_wire_str(),
                r.host,
            );
        }
        None => (octo_core::now_secs(), false),
    };

    // Phase 1: wait for the agent container to publish its identity. The
    // VM may still be cloud-initting on the first connection attempt.
    info!("[lair/register_remote_agent] {host}: waiting for agent-info.json");
    let info = match ssh_ops::await_agent_info(
        &host,
        "root",
        &key_path,
        Duration::from_secs(300),
        Duration::from_secs(8),
    ).await {
        Ok(i) => i,
        Err(e) => return format!("error: could not pull agent info from {host}: {e:#}"),
    };

    // Insert (or refresh) a Pending row now that we know the agent's
    // pubkey + port. Mobile will pick this up on the next poll and surface
    // an in-progress agent. Resumption on subsequent register calls will
    // hit this row and skip straight to the SSH phases.
    let image = std::env::var("OCTO_AGENT_IMAGE")
        .unwrap_or_else(|_| docker_ops::DEFAULT_AGENT_IMAGE.to_string());
    {
        let pending = AgentRecord {
            name:          name.clone(),
            container_id:  None,
            host:          Some(host.clone()),
            port:          info.port,
            pubkey:        info.pubkey.clone(),
            git_url:       git_url.clone(),
            status:        AgentStatus::Pending,
            image_version: image.clone(),
            created_at,
            last_seen:     octo_core::now_secs(),
            instance_id:   instance_id.clone(),
            provider:      provider.clone(),
            metadata:      metadata.clone(),
        };
        if let Err(e) = state.registry.lock().unwrap().set(pending) {
            return format!("error inserting pending registry row: {e:#}");
        }
        state.poll_trigger.notify_one();
    }

    // Phase 2: drop /data/config.json with the operator's API keys. The
    // agent re-reads this on every model call (`resolve_api_key` /
    // `resolve_model`) so we don't have to restart yet for credentials —
    // but we will restart for the workspace clone below.
    let cfg = serde_json::json!({
        "name":              null,
        "anthropic_api_key": std::env::var("ANTHROPIC_API_KEY").ok(),
        "openai_api_key":    std::env::var("OPENAI_API_KEY").ok(),
        "model":             std::env::var("MODEL").ok(),
        "api_url":           std::env::var("OPENAI_API_URL").ok(),
    });
    let cfg_str = match serde_json::to_string_pretty(&cfg) {
        Ok(s) => s,
        Err(e) => return format!("error encoding config.json: {e:#}"),
    };
    info!("[lair/register_remote_agent] {host}: dropping /var/lib/octo/agent-data/config.json");
    if let Err(e) = ssh_ops::write_file(
        &host, "root", &key_path,
        "/var/lib/octo/agent-data/config.json",
        &cfg_str,
        0o600,
    ).await {
        return format!(
            "error writing config.json to {host}: {e:#}. \
             Re-run `register_remote_agent(name='{name}', host='{host}', ...)` to retry; \
             the registry row stays Pending and the SSH phases are idempotent.",
        );
    }

    // Phase 3: if the user gave a git_url, clone into the workspace. Token
    // is interpolated into the script body and piped over SSH stdin — never
    // lands on disk on the lair side. (The credential.helper config the
    // script installs DOES persist the token in the VM's workspace .git/config;
    // that's load-bearing for `git push` and can't be avoided.)
    if let Some(url) = git_url.clone() {
        let token = std::env::var("GH_TOKEN").unwrap_or_default();
        let user_name  = std::env::var("GIT_USER_NAME") .unwrap_or_else(|_| "octo".to_string());
        let user_email = std::env::var("GIT_USER_EMAIL").unwrap_or_else(|_| "octo@localhost".to_string());
        let script = build_remote_clone_script(&url, &token, &user_name, &user_email);
        info!("[lair/register_remote_agent] {host}: cloning {url}");
        if let Err(e) = ssh_ops::run_script(&host, "root", &key_path, &script).await {
            return format!(
                "error cloning git repo on {host}: {e:#}. \
                 Re-run `register_remote_agent` to retry (row stays Pending).",
            );
        }
    }

    // Phase 4: docker restart octo-agent so the workspace + config.json are
    // picked up cleanly (correct system prompt for repo-bound agents).
    info!("[lair/register_remote_agent] {host}: restarting octo-agent");
    if let Err(e) = ssh_ops::run_script(
        &host, "root", &key_path,
        "set -e; docker restart octo-agent >/dev/null",
    ).await {
        return format!(
            "error restarting octo-agent on {host}: {e:#}. \
             Re-run `register_remote_agent` to retry (row stays Pending).",
        );
    }

    // Re-pull agent-info to confirm the restart completed. Same file path;
    // the agent rewrites it on every boot. Shorter timeout — the restart
    // should land within ~30 s. Soft-fail: if the second poll times out
    // (e.g. agent-info.json takes a moment to be re-written), keep the
    // pre-restart pubkey/port — they should be unchanged anyway.
    info!("[lair/register_remote_agent] {host}: confirming restart");
    let info = match ssh_ops::await_agent_info(
        &host,
        "root",
        &key_path,
        Duration::from_secs(60),
        Duration::from_secs(4),
    ).await {
        Ok(i) => i,
        Err(_) => {
            warn!("[lair/register_remote_agent] {host}: restart confirmation timed out; using pre-restart info");
            info
        }
    };

    let record = AgentRecord {
        name:          name.clone(),
        container_id:  None,
        host:          Some(host.clone()),
        port:          info.port,
        pubkey:        info.pubkey.clone(),
        git_url:       git_url.clone(),
        status:        AgentStatus::Running,
        image_version: image,
        created_at,
        last_seen:     octo_core::now_secs(),
        instance_id,
        provider,
        metadata,
    };
    if let Err(e) = state.registry.lock().unwrap().set(record) {
        return format!("error finalising registry row for '{name}': {e:#}");
    }
    state.poll_trigger.notify_one();
    let verb = if resuming { "Resumed and registered" } else { "Registered" };
    format!(
        "{verb} remote agent '{name}' at {host}:{} (pubkey={}). \
         Mobile will pick it up via the next containers event.",
        info.port, info.pubkey,
    )
}

/// Build a bash blob that clones (or refreshes) `url` into the remote agent's
/// workspace and wires the git user identity + credential helper. Token is
/// interpolated verbatim; the script is piped over SSH stdin so it never
/// lands on disk on the lair side.
fn build_remote_clone_script(
    url:        &str,
    gh_token:   &str,
    user_name:  &str,
    user_email: &str,
) -> String {
    // Compute the clone URL with the token spliced in for HTTPS. We do this
    // on lair so the remote script doesn't need to know token-handling rules.
    let clone_url = if url.starts_with("https://") && !gh_token.is_empty() {
        let rest = url.trim_start_matches("https://");
        let rest = match rest.find('@') {
            Some(i) => &rest[i + 1..],
            None    => rest,
        };
        format!("https://x-token:{gh_token}@{rest}")
    } else {
        url.to_string()
    };
    let credential_helper = if !gh_token.is_empty() && url.starts_with("https://") {
        Some(format!("!f() {{ echo username=x-token; echo password={gh_token}; }}; f"))
    } else {
        None
    };

    let mut script = String::new();
    script.push_str("set -e\n");
    script.push_str("WORKSPACE=/var/lib/octo/agent-workspace\n");
    script.push_str(&format!("CLONE_URL='{}'\n", clone_url.replace('\'', "'\\''")));
    script.push_str(&format!("USER_NAME='{}'\n",  user_name.replace('\'', "'\\''")));
    script.push_str(&format!("USER_EMAIL='{}'\n", user_email.replace('\'', "'\\''")));
    script.push_str(r#"if [ -d "$WORKSPACE/.git" ]; then
    git -C "$WORKSPACE" remote set-url origin "$CLONE_URL"
    git -C "$WORKSPACE" fetch --all
else
    # Workspace may be empty or hold a stale directory from a previous run.
    find "$WORKSPACE" -mindepth 1 -maxdepth 1 -exec rm -rf {} + 2>/dev/null || true
    git clone "$CLONE_URL" "$WORKSPACE"
fi
git -C "$WORKSPACE" config user.name  "$USER_NAME"
git -C "$WORKSPACE" config user.email "$USER_EMAIL"
"#);
    if let Some(helper) = credential_helper {
        script.push_str(&format!("HELPER='{}'\n", helper.replace('\'', "'\\''")));
        script.push_str(r#"git -C "$WORKSPACE" config credential.helper "$HELPER"
"#);
    }
    script
}

async fn exec_forget_agent(state: Arc<AppState>, input: serde_json::Value) -> String {
    let name = match input.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return "error: missing 'name' field".to_string(),
    };

    let record = state.registry.lock().unwrap().get(&name).cloned();
    let record = match record {
        Some(r) => r,
        None    => return format!("'{name}' was not in the registry"),
    };
    if !record.is_remote() {
        return format!(
            "error: '{name}' is a local agent. Use `terminate_agent` instead so the Docker \
             container + volumes are also cleaned up."
        );
    }

    let removed = state.registry.lock().unwrap().remove(&name);
    match removed {
        Ok(true) => {
            state.poll_trigger.notify_one();
            format!("Forgot '{name}' — registry row removed; no Docker or VM action taken.")
        }
        Ok(false) => format!("'{name}' was not in the registry"),
        Err(e)    => format!("error: {e:#}"),
    }
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
    info!("[lair/run_background_task] spawning {task_id} ({} chars)", task_description.len());

    // Build a fresh tools+executor pair so the background task gets the same
    // capabilities the main loop has, including MCP servers.
    let extra_tools = build_tools_with_mcp(&state.mcp_pool, &lair_extra_tools()).await;
    let executor    = chain_executor_with_mcp(state.mcp_pool.clone(), lair_extra_executor(state.clone()));

    // Register the task *before* spawning so the per-chat registry is in place
    // by the time the deliver closure can possibly run. The cancel token is
    // shared between the registry (so the user can stop it) and the spawn.
    let cancel = CancellationToken::new();
    register_task(&state.stream_state, &data_dir(), TaskRecord {
        task_id:          task_id.clone(),
        task_description: task_description.clone(),
        status:           TaskStatus::Running,
        started_at:       now_secs(),
        completed_at:     None,
        summary:          None,
        cost_usd:         None,
    }, cancel.clone());
    buffer_and_fanout(&state.stream_state, tasks_wire_json(&state.stream_state));

    let params = BackgroundTaskParams {
        task_id:          task_id.clone(),
        task_description,
        system:           state.system.clone(),
        model,
        api_key,
        cwd:              "/".to_string(),
        extra_tools,
        extra_executor:   executor,
    };

    let stream_state_arc = state.clone();
    spawn_background_task(params, cancel, move |outcome| {
        finalize_task(&stream_state_arc.stream_state, &data_dir(), &outcome);
        buffer_and_fanout(&stream_state_arc.stream_state, tasks_wire_json(&stream_state_arc.stream_state));
        // Persist a `bg_complete` row so the model sees the result on its next
        // turn (auto-triggered below if no foreground turn is mid-stream).
        // Translated to user-role text at API serialisation time so providers
        // see it as ordinary input.
        let injection = format!(
            "Background task {} completed (status={}). Original task: {}\n\nResult:\n{}",
            outcome.task_id, outcome.status, outcome.task_description, outcome.summary
        );
        {
            let mut msgs = stream_state_arc.messages.lock().unwrap();
            msgs.push(ApiMessage {
                role:    "bg_complete".to_string(),
                content: vec![ContentBlock::Text { text: injection.clone() }],
            });
            save_messages(&msgs);
        }

        // Live UI signal: emit a `bg_complete` wire frame so connected clients
        // can render the chip between assistant turns. /history reload would
        // surface the same row eventually; this just makes it visible
        // immediately. Stable task_id keys deduplicate against /history.
        let bg_event = ChatEvent::BgComplete {
            task_id: outcome.task_id.clone(),
            text:    injection,
        };
        if let Some(json) = chat_event_to_wire_json(&bg_event) {
            buffer_and_fanout(&stream_state_arc.stream_state, json.to_string());
        }

        // Transient system-event fan-out (kept for status banner display).
        let event = completion_chat_event(&outcome);
        if let Some(json) = chat_event_to_wire_json(&event) {
            buffer_and_fanout(&stream_state_arc.stream_state, json.to_string());
        }

        // Best-effort push notification. Wakes registered devices even when
        // the WS isn't connected (app suspended). Mobile fetches the actual
        // outcome from /history over the Noise tunnel after waking.
        let signer = stream_state_arc.relay_signer.clone();
        let url    = stream_state_arc.relay_url.clone();
        if !url.is_empty() {
            let title = format!("Background task {}", outcome.status);
            let body  = outcome.summary.chars().take(120).collect::<String>();
            tokio::spawn(async move {
                relay_client::notify(&url, &signer, "task_complete", Some(&title), Some(&body)).await;
            });
        }

        // Kick off an auto-turn so the model can react. If a foreground turn
        // is in flight this is a no-op; the post-turn `try_continue_auto` in
        // spawn_turn will pick the row up.
        try_continue_auto(stream_state_arc.clone());
    });

    format!("Background task {task_id} started. The user will be notified when it completes.")
}

async fn exec_restart_all_containers(state: Arc<AppState>) -> String {
    let names: Vec<String> = state.registry.lock().unwrap()
        .list().iter().map(|r| r.name.clone()).collect();
    if names.is_empty() {
        info!("[lair/restart_all] no agents found");
        return "No agents found to restart.".to_string();
    }
    let mut restarted = Vec::new();
    for name in &names {
        if let Err(e) = docker_ops::stop_container(&state.docker, name).await {
            warn!("[lair/restart_all] stop {name}: {e:#}");
        }
        match docker_ops::start_container(&state.docker, name).await {
            Ok(_)  => restarted.push(name.clone()),
            Err(e) => error!("[lair/restart_all] start {name}: {e:#}"),
        }
    }
    state.poll_trigger.notify_one();
    info!("[lair/restart_all] restarted: {}", restarted.join(", "));
    format!("Restarted: {}.", restarted.join(", "))
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
        warn!("[lair] DEV MODE: using fixed dev keypair");
        (DEV_STATIC_PRIVATE.to_vec(), DEV_STATIC_PUBLIC.to_vec())
    } else if let Some(kp) = injected_keypair {
        kp
    } else {
        load_or_generate_keypair(&key_file)
    };

    let pubkey_b32 = to_base32(&static_public);
    // Hex-encode the 64-byte keypair so it can be injected into children as an env var.
    let noise_private_key_hex = {
        let mut combined = static_private.clone();
        combined.extend_from_slice(&static_public);
        hex::encode(&combined)
    };
    let noise_port: u16 = std::env::var("NOISE_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(9000);
    let public_port: u16 = std::env::var("PUBLIC_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(noise_port);
    let http_port:  u16 = 8000;
    let public_host = crate::bootstrap::resolve_public_host("lair").await?;
    crate::bootstrap::run_startup_script("lair").await?;

    info!("[lair] noise_pubkey={pubkey_b32} noise_port={noise_port} http_port={http_port} public_host={public_host}");

    let docker = docker_ops::build_client()
        .map_err(|e| anyhow::anyhow!("failed to initialize Docker client: {e:#}"))?;
    info!("[lair] Docker client initialized");

    tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

    let dir = data_dir();
    fs::create_dir_all(&dir).ok();

    // Generate an SSH keypair for ops backchannels (e.g. tailing logs on a
    // remote-provisioned VM). Idempotent — existing keys are left untouched.
    match ensure_ssh_keypair(&dir) {
        Ok((priv_path, _pub_path)) => info!("[lair] SSH keypair ready at {}", priv_path.display()),
        Err(e) => warn!("[lair] could not ensure SSH keypair: {e:#}"),
    }

    let registry = Registry::load(dir.join("agents.json"))
        .map_err(|e| anyhow::anyhow!("load agent registry: {e:#}"))?;
    info!("[lair] loaded agent registry: {} entries", registry.list().len());
    let registry = Arc::new(Mutex::new(registry));

    let messages = load_messages();
    info!("[lair] loaded {} message(s) from history", messages.len());

    let mcp_json_path = dir.join("mcp.json");
    if !mcp_json_path.exists() {
        if let Ok(json) = std::env::var("MCP_CONFIG_JSON") {
            if let Err(e) = fs::write(&mcp_json_path, &json) {
                warn!("[lair] failed to seed mcp.json: {e}");
            } else {
                info!("[lair] seeded mcp.json from MCP_CONFIG_JSON secret");
            }
        }
    }

    let mcp_pool     = init_mcp_pool().await;
    let poll_trigger = Arc::new(Notify::new());
    let (containers_tx, containers_rx) = watch::channel(Vec::<ContainerInfo>::new());
    let (ready_tx, ready_rx)           = watch::channel(false);

    let relay_signer  = Arc::new(RelaySigner::load_or_generate(RELAY_SIGNING_KEY_FILE));
    let relay_url_str = std::env::var("OCTO_RELAY_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RELAY_URL.to_string());
    info!("[lair] relay_signing_pubkey={} relay_url={}", relay_signer.pubkey_b32(), relay_url_str);

    let state = Arc::new(AppState {
        messages:              Arc::new(Mutex::new(messages)),
        last_cost_usd:         Mutex::new(None),
        system:                build_system_prompt(),
        containers_tx,
        containers_rx,
        poll_trigger:          poll_trigger.clone(),
        pubkey_b32:            pubkey_b32.clone(),
        noise_private_key_hex,
        public_host:           public_host.clone(),
        docker,
        registry,
        mcp_pool,
        cancel:                Mutex::new(CancellationToken::new()),
        is_streaming:          AtomicBool::new(false),
        stream_state:          Mutex::new({
            let mut ss = StreamState::new();
            ss.tasks = octo_core::load_tasks(&data_dir(), "lair");
            ss
        }),
        ready_rx,
        relay_signer,
        relay_url:             relay_url_str,
    });

    tokio::spawn(poll_containers(state.clone(), ready_tx.clone()));

    // Soft cap so a stuck poller can't keep the UI in "connecting" forever.
    // 30s is well past the poller's 5s warm-up + a normal first list, but
    // short enough that mobile recovers if Docker is unreachable.
    let ready_tx_timeout = ready_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if !*ready_tx_timeout.borrow() {
            warn!("[lair] readiness latch timed out after 30s — flipping ready anyway");
            ready_tx_timeout.send_replace(true);
        }
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let app = Router::new()
        .route("/health",           get(health_handler))
        .route("/info",             get(info_handler))
        .route("/history",          get(history_handler))
        .route("/stream",           get(stream_handler))
        .route("/interrupt",        post(interrupt_handler))
        .route("/clear",            post(clear_handler))
        .with_state(state)
        .layer(cors);

    let addr = format!("0.0.0.0:{http_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await
        .map_err(|e| anyhow::anyhow!("failed to bind HTTP port {addr}: {e}"))?;
    info!("[lair] HTTP listening on {addr} (Noise proxy on 0.0.0.0:{noise_port})");

    // Listener is bound; the Noise port is reachable. Print the QR now so the
    // user never scans before the server can accept the connection.
    crate::bootstrap::print_qr("lair", &public_host, public_port, &pubkey_b32);

    axum::serve(listener, app).await
        .map_err(|e| anyhow::anyhow!("axum serve error: {e}"))?;
    Ok(())
}
