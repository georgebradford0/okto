//! Lair (parent / orchestrator) role.
//!
//! Lair runs on the operator's host as a plain OS process. It:
//!   - listens for mobile clients over Noise on `NOISE_PORT` (default 9000),
//!     forwarding the encrypted stream to its own HTTP server on 127.0.0.1:8000;
//!   - spawns child agent processes via `AgentSupervisor` and tracks them in
//!     a JSON registry at `<OCTO_DATA_DIR>/agents.json`;
//!   - proxies mobile WebSocket traffic to a chosen child via `/agents/:name/stream`.

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
        Path as AxumPath, State,
    },
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use octo_core::{
    self,
    build_tools_with_mcp, chain_executor_with_mcp,
    cancel_task as core_cancel_task, completion_chat_event, ensure_ssh_keypair, finalize_task,
    from_base32, init_mcp_pool, init_shell_env, load_or_generate_keypair, now_secs,
    open_noise_tunnel, record_task_progress,
    register_task, tasks_wire_json, TaskRecord, TaskStatus,
    relay as relay_client, RelaySigner,
    resolve_api_key, resolve_model, run_noise_proxy, run_command_in_background_tool, send_message,
    spawn_background_command, to_base32, ApiMessage, AnthropicTool, BackgroundCommandParams, ChatEvent,
    ContentBlock, McpPool, DEV_PUBKEY_BASE32, DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC,
    KEEPALIVE_INTERVAL, KEEPALIVE_MAX_MISSED,
    StreamState, buffer_and_fanout, chat_event_to_wire_json, messages_to_history,
    parse_ping_id, parse_pong_id,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch, Notify};
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};

use crate::agent_proc::{AgentSupervisor, SpawnParams};
use crate::ssh as ssh_ops;
use octo_core::{AgentRecord, AgentStatus, Registry, status_from_alive};

const RELAY_SIGNING_KEY_FILE: &str = "relay_signing_key.bin";
const DEFAULT_RELAY_URL:      &str = "https://octorelay.directto.link";

fn data_dir() -> PathBuf { octo_core::data_dir() }

/// Wire-shape pushed to mobile as part of an `agents` event. Just identity
/// + status — no host/port/pubkey because mobile only ever talks to lair
/// and reaches children through `/agents/:name/stream` proxy URLs.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct AgentWire {
    id:     String,
    name:   String,
    status: String,
    /// `"local"` or `"remote"`. Surfaced so the mobile sidebar can label
    /// remote agents distinctly if it wants — purely advisory.
    kind:   &'static str,
}

// ── Session persistence ───────────────────────────────────────────────────────

fn save_messages(messages: &[ApiMessage]) {
    octo_core::save_messages(&data_dir(), messages, "lair");
}

fn load_messages() -> Vec<ApiMessage> {
    octo_core::load_messages(&data_dir(), "lair")
}

// ── App state ─────────────────────────────────────────────────────────────────

struct AppState {
    messages:      Arc<Mutex<Vec<ApiMessage>>>,
    last_cost_usd: Mutex<Option<f64>>,
    system:        String,
    /// Watch channel published by the agent poller. Each /stream WS subscribes
    /// and re-sends an `agents` event whenever the list changes.
    agents_tx:     watch::Sender<Vec<AgentWire>>,
    agents_rx:     watch::Receiver<Vec<AgentWire>>,
    poll_trigger:  Arc<Notify>,
    pubkey_b32:    String,
    #[allow(dead_code)]
    public_host:   String,
    /// Lair's Noise static private key. Used as the initiator key when
    /// opening outbound Noise tunnels to remote agents.
    lair_priv:     Vec<u8>,
    supervisor:    Arc<AgentSupervisor>,
    registry:      Arc<Mutex<Registry>>,
    mcp_pool:      McpPool,
    cancel:        Mutex<CancellationToken>,
    is_streaming:  AtomicBool,
    stream_state:  Mutex<StreamState>,
    /// Flips to true once subsystem init completes (first agent poll done).
    ready_rx:      watch::Receiver<bool>,
    relay_signer:  Arc<RelaySigner>,
    relay_url:     String,
    /// Management API bearer token. Set from `LAIR_MGMT_TOKEN` env var at
    /// startup. When `Some(_)`, every state-mutating CLI endpoint requires
    /// the matching `X-Octo-Token` header — peer processes inside the
    /// container (i.e. child agents) don't get the token in their env
    /// (`agent_proc::spawn` strips it) and run as a different uid so
    /// they can't read `/proc/1/environ` either. When `None`, the
    /// management API is open (useful for ad-hoc `docker run` without
    /// the CLI).
    mgmt_token:    Option<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse { (StatusCode::OK, "ok") }

/// Bearer-token middleware for the management endpoints. When
/// `state.mgmt_token` is `Some(_)`, every request must carry a matching
/// `X-Octo-Token` header. When `None` (no token configured at startup),
/// the middleware is a no-op — useful for `docker run` smoke tests
/// without minting a token first.
async fn require_mgmt_token(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(expected) = state.mgmt_token.as_deref() else {
        return next.run(req).await;
    };
    let supplied = req.headers()
        .get("x-octo-token")
        .and_then(|v| v.to_str().ok());
    if supplied != Some(expected) {
        return (StatusCode::FORBIDDEN, "missing or invalid X-Octo-Token").into_response();
    }
    next.run(req).await
}

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

enum TurnTrigger { User(String), Auto }

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

        let cancel = CancellationToken::new();
        *state.cancel.lock().unwrap() = cancel.clone();

        state.stream_state.lock().unwrap().buffer.clear();

        let extra_tools = build_tools_with_mcp(&state.mcp_pool, &lair_extra_tools()).await;
        let executor    = chain_executor_with_mcp(state.mcp_pool.clone(), lair_extra_executor(Arc::clone(&state)));

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

        while let Some(event) = event_rx.recv().await {
            if let Some(json) = chat_event_to_wire_json(&event) {
                buffer_and_fanout(&state.stream_state, json.to_string());
            }
        }
        state.is_streaming.store(false, Ordering::Relaxed);
        state.stream_state.lock().unwrap().buffer.clear();
        info!("[lair/stream] turn complete, is_streaming=false");
        try_continue_auto(state.clone());
    });
}

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
        return;
    }
    info!("[lair/stream] auto-turn triggered by bg_complete");
    spawn_turn(state, TurnTrigger::Auto);
}

async fn handle_stream(socket: WebSocket, state: Arc<AppState>) {
    info!("[lair/stream] WebSocket connection opened");
    let (mut ws_tx, mut ws_rx) = socket.split();

    let mut ready_rx = state.ready_rx.clone();
    while !*ready_rx.borrow() {
        if ready_rx.changed().await.is_err() { break; }
    }

    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<String>();
    let (replay, resumed) = {
        let mut ss = state.stream_state.lock().unwrap();
        ss.subs.push(sub_tx);
        let resumed = state.is_streaming.load(Ordering::Relaxed);
        let replay = if resumed { ss.buffer.clone() } else { Vec::new() };
        (replay, resumed)
    };

    let ready = serde_json::json!({"type":"ready","session_id":"","resumed":resumed}).to_string();
    if ws_tx.send(WsMessage::Text(ready)).await.is_err() {
        return;
    }
    {
        let snapshot = state.agents_rx.borrow().clone();
        let json = serde_json::json!({"type":"agents","agents":snapshot}).to_string();
        if ws_tx.send(WsMessage::Text(json)).await.is_err() {
            return;
        }
    }
    if ws_tx.send(WsMessage::Text(tasks_wire_json(&state.stream_state))).await.is_err() {
        return;
    }
    if !replay.is_empty() {
        info!("[lair/stream] replaying {} buffered event(s) to new connection", replay.len());
        for event in replay {
            if ws_tx.send(WsMessage::Text(event)).await.is_err() { return; }
        }
    }

    let mut agents_rx = state.agents_rx.clone();

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

            res = agents_rx.changed() => {
                if res.is_err() { break; }
                let list = agents_rx.borrow_and_update().clone();
                let json = serde_json::json!({"type":"agents","agents":list}).to_string();
                if ws_tx.send(WsMessage::Text(json)).await.is_err() { break; }
            },

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

    info!("[lair/stream] connection closed");
}

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
            buffer_and_fanout(&state.stream_state, serde_json::json!({"type":"interrupt_ack"}).to_string());
        }
        "start_agent" => {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if id.is_empty() { warn!("[lair/stream] start_agent missing id"); return; }
            info!("[lair/stream] start_agent id={id}");
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(e) = start_agent_by_name(&state, &id).await {
                    error!("[lair/stream] start_agent failed: {e}");
                    let json = serde_json::json!({"type":"error","message":format!("start_agent: {e}")}).to_string();
                    buffer_and_fanout(&state.stream_state, json);
                }
            });
        }
        "terminate_agent" => {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if id.is_empty() { warn!("[lair/stream] terminate_agent missing id"); return; }
            info!("[lair/stream] terminate_agent id={id}");
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(e) = terminate_agent_by_name(&state, &id).await {
                    error!("[lair/stream] terminate_agent failed: {e}");
                    let json = serde_json::json!({"type":"error","message":format!("terminate_agent: {e}")}).to_string();
                    buffer_and_fanout(&state.stream_state, json);
                }
            });
        }
        "cancel_task" => {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if id.is_empty() { warn!("[lair/stream] cancel_task missing id"); return; }
            let fired = core_cancel_task(&state.stream_state, &id);
            info!("[lair/stream] cancel_task id={id} fired={fired}");
        }
        "pong" => {}
        other => warn!("[lair/stream] unknown client frame type='{other}'"),
    }
}

async fn clear_handler(State(state): State<Arc<AppState>>) -> StatusCode {
    info!("[lair/clear] clearing conversation history");
    let mut msgs = state.messages.lock().unwrap();
    msgs.clear();
    save_messages(&msgs);
    StatusCode::OK
}

/// Re-spawn a stopped agent by name. Re-uses its existing data_dir/workspace.
async fn start_agent_by_name(state: &AppState, name: &str) -> Result<(), String> {
    let record = state.registry.lock().unwrap().get(name).cloned()
        .ok_or_else(|| format!("agent '{name}' not found"))?;
    if record.is_remote() {
        return Err(format!(
            "agent '{name}' is a remote agent — start/stop is managed by the cloud \
             provider, not lair. Use the provisioning MCP to bring its VM up/down."
        ));
    }
    let cfg = octo_core::read_config();
    let gh_token = std::env::var("GH_TOKEN").ok().filter(|s| !s.is_empty());
    // git_url isn't stored in the registry — the workspace dir already holds
    // the clone (if any), and `bootstrap::ensure_workspace` detects it on
    // restart via the `.git` marker.
    let params = SpawnParams {
        name:              &record.name,
        port:              record.port,
        git_url:           None,
        startup_script:    None,
        startup_prompt:    None,
        anthropic_api_key: cfg.anthropic_api_key.as_deref(),
        openai_api_key:    cfg.openai_api_key.as_deref(),
        openai_api_url:    cfg.api_url.as_deref(),
        model:             cfg.model.as_deref(),
        gh_token:          gh_token.as_deref(),
        agent_purpose:     None,
    };
    let pid = state.supervisor.spawn(&params).await.map_err(|e| e.to_string())?;
    {
        let mut reg = state.registry.lock().unwrap();
        let _ = reg.update_pid(name, Some(pid));
        let _ = reg.update_status(name, AgentStatus::Pending);
    }
    state.poll_trigger.notify_one();
    Ok(())
}

/// Stop and remove a child agent: SIGTERM the process, drop the per-agent
/// data/workspace dirs, and remove the registry row.
async fn terminate_agent_by_name(state: &AppState, name: &str) -> Result<(), String> {
    let is_remote = {
        let reg = state.registry.lock().unwrap();
        match reg.get(name) {
            Some(r) => r.is_remote(),
            None    => return Err(format!("agent '{name}' not found")),
        }
    };
    if is_remote {
        return Err(format!(
            "'{name}' is a remote agent — terminate_agent only destroys local processes. \
             Use the provisioning MCP's terminate-instance method first, then call \
             forget_agent('{name}') to clean up the registry row."
        ));
    }
    state.supervisor.terminate(name).await.map_err(|e| e.to_string())?;
    {
        let mut reg = state.registry.lock().unwrap();
        let _ = reg.remove(name);
    }
    state.poll_trigger.notify_one();
    Ok(())
}

// ── Agent poller ──────────────────────────────────────────────────────────────

async fn poll_agents(state: Arc<AppState>, ready_tx: watch::Sender<bool>) {
    info!("[agents] poller starting, initial delay 2s");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut first_iter = true;
    loop {
        debug!("[agents] reconciling registry against pid liveness");
        let new_agents: Vec<AgentWire> = {
            let mut reg = state.registry.lock().unwrap();
            let now = octo_core::now_secs();
            let snapshot = reg.list().to_vec();
            let mut out = Vec::with_capacity(snapshot.len());
            for record in snapshot {
                let kind = if record.is_remote() { "remote" } else { "local" };
                let status = if record.is_remote() {
                    // Remote agents aren't continuously probed — surface
                    // whatever status the registration / terminate flow set.
                    record.status
                } else {
                    let alive = record.pid
                        .map(AgentSupervisor::is_alive)
                        .unwrap_or(false);
                    let s = status_from_alive(alive);
                    let _ = reg.update_status(&record.name, s);
                    if alive { let _ = reg.update_last_seen(&record.name, now); }
                    s
                };
                out.push(AgentWire {
                    id:     record.name.clone(),
                    name:   record.name.clone(),
                    status: status.as_wire_str().to_string(),
                    kind,
                });
            }
            out
        };

        let changed = *state.agents_tx.borrow() != new_agents;
        if changed {
            let n = new_agents.len();
            let names = new_agents.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ");
            info!("[agents] state changed: {n} child(ren): {names}");
            state.agents_tx.send_replace(new_agents);
        }

        if first_iter {
            first_iter = false;
            ready_tx.send_replace(true);
            info!("[agents] first poll complete — server marked ready");
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {
                debug!("[agents] poll interval elapsed");
            }
            _ = state.poll_trigger.notified() => {
                info!("[agents] poll triggered manually");
            }
        }
    }
}

// ── Network probes ────────────────────────────────────────────────────────────

/// Probe `host:port` with a short per-attempt TCP `connect` and retry up to
/// `total_timeout`. Returns `Ok(())` on the first successful connect or
/// `Err(_)` with the last connect error after exhausting the deadline.
///
/// Used by `exec_register_remote_agent` to fail fast on unreachable hosts
/// (wrong IP, bad security group, subnet without IGW route, terminated
/// instance) before falling through to the multi-minute SSH wait loop.
async fn probe_tcp_reachable(host: &str, port: u16, total_timeout: Duration) -> Result<(), String> {
    let addr = format!("{host}:{port}");
    let deadline  = tokio::time::Instant::now() + total_timeout;
    let attempt_t = Duration::from_secs(5);
    let mut last_err = String::from("no attempts made");
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(
            attempt_t,
            tokio::net::TcpStream::connect(&addr),
        ).await {
            Ok(Ok(_))  => return Ok(()),
            Ok(Err(e)) => last_err = e.to_string(),
            Err(_)     => last_err = format!("TCP connect timed out after {attempt_t:?}"),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Err(last_err)
}

// ── Agent proxy (mobile <-> lair <-> agent) ───────────────────────────────────

/// HTTP forward helper: take the request method + body, send it to
/// `http://127.0.0.1:<child_port>/<sub_path>`, and copy the response back.
async fn forward_http(
    method:    reqwest::Method,
    child_url: &str,
    body:      Option<serde_json::Value>,
) -> Response {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let mut req = client.request(method, child_url);
    if let Some(b) = body { req = req.json(&b); }
    match req.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let body = resp.bytes().await.unwrap_or_default();
            (status, body).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("proxy error: {e}")).into_response(),
    }
}

/// Build the base URL lair should hit to make an HTTP call against a child
/// agent. For local agents this is just `http://127.0.0.1:<port>`. For
/// remote agents we spin up a one-shot outbound Noise tunnel; reqwest will
/// open one TCP connection to the loopback ephemeral port, the tunnel
/// pipes it through Noise to the remote VM, and the listener closes after
/// the single connection.
async fn child_http_base(record: &AgentRecord, lair_priv: Vec<u8>) -> Result<String, String> {
    if let Some(host) = &record.host {
        let pubkey_b32 = record.pubkey.as_deref().unwrap_or_default();
        if pubkey_b32.is_empty() {
            return Err(format!("remote agent '{}' has no recorded pubkey", record.name));
        }
        let expected_pubkey = from_base32(pubkey_b32).ok_or_else(|| {
            format!("remote agent '{}' has malformed pubkey", record.name)
        })?;
        let local_port = open_noise_tunnel(
            host.clone(),
            record.port,
            expected_pubkey,
            lair_priv,
        ).await.map_err(|e| format!("open noise tunnel to {host}:{}: {e}", record.port))?;
        Ok(format!("http://127.0.0.1:{local_port}"))
    } else {
        Ok(format!("http://127.0.0.1:{}", record.port))
    }
}

async fn lookup_record(state: &AppState, name: &str) -> Result<AgentRecord, Response> {
    state.registry.lock().unwrap().get(name).cloned()
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("agent '{name}' not found")).into_response())
}

async fn proxy_agent_http(
    state:   &Arc<AppState>,
    name:    &str,
    method:  reqwest::Method,
    sub:     &str,
) -> Response {
    let record = match lookup_record(state, name).await {
        Ok(r)  => r,
        Err(r) => return r,
    };
    let base = match child_http_base(&record, state.lair_priv.clone()).await {
        Ok(u)  => u,
        Err(e) => return (StatusCode::BAD_GATEWAY, e).into_response(),
    };
    forward_http(method, &format!("{base}{sub}"), None).await
}

async fn proxy_agent_history(
    AxumPath(name): AxumPath<String>,
    State(state):   State<Arc<AppState>>,
) -> Response {
    proxy_agent_http(&state, &name, reqwest::Method::GET, "/history").await
}

async fn proxy_agent_interrupt(
    AxumPath(name): AxumPath<String>,
    State(state):   State<Arc<AppState>>,
) -> Response {
    proxy_agent_http(&state, &name, reqwest::Method::POST, "/interrupt").await
}

async fn proxy_agent_clear(
    AxumPath(name): AxumPath<String>,
    State(state):   State<Arc<AppState>>,
) -> Response {
    proxy_agent_http(&state, &name, reqwest::Method::POST, "/clear").await
}

async fn proxy_agent_branches(
    AxumPath(name): AxumPath<String>,
    State(state):   State<Arc<AppState>>,
) -> Response {
    proxy_agent_http(&state, &name, reqwest::Method::GET, "/branches").await
}

async fn proxy_agent_stream_handler(
    AxumPath(name): AxumPath<String>,
    ws:             WebSocketUpgrade,
    State(state):   State<Arc<AppState>>,
) -> Response {
    let record = {
        let reg = state.registry.lock().unwrap();
        match reg.get(&name) {
            Some(r) => r.clone(),
            None    => return (StatusCode::NOT_FOUND, format!("agent '{name}' not found")).into_response(),
        }
    };
    let lair_priv = state.lair_priv.clone();
    ws.on_upgrade(move |client_ws| proxy_to_child(client_ws, record, lair_priv))
}

/// Open the localhost URL lair should connect to in order to reach a child
/// agent's `/stream`. For local agents that's just `ws://127.0.0.1:<port>`.
/// For remote agents we spin up an outbound Noise tunnel first, returning a
/// loopback URL that tunnels into the remote VM's encrypted Noise port.
async fn resolve_child_ws_url(record: &AgentRecord, lair_priv: Vec<u8>) -> Result<String, String> {
    if let Some(host) = &record.host {
        let pubkey_b32 = record.pubkey.as_deref().unwrap_or_default();
        if pubkey_b32.is_empty() {
            return Err(format!(
                "remote agent '{}' has no recorded pubkey — registration may not have \
                 completed; re-run register_remote_agent",
                record.name,
            ));
        }
        let expected_pubkey = from_base32(pubkey_b32).ok_or_else(|| {
            format!("remote agent '{}' has malformed pubkey", record.name)
        })?;
        let local_port = open_noise_tunnel(
            host.clone(),
            record.port,
            expected_pubkey,
            lair_priv,
        ).await.map_err(|e| format!("open noise tunnel to {host}:{}: {e}", record.port))?;
        Ok(format!("ws://127.0.0.1:{local_port}/stream"))
    } else {
        Ok(format!("ws://127.0.0.1:{}/stream", record.port))
    }
}

async fn proxy_to_child(mobile_ws: WebSocket, record: AgentRecord, lair_priv: Vec<u8>) {
    use tokio_tungstenite::tungstenite::Message as TMessage;

    let name = record.name.clone();
    let url = match resolve_child_ws_url(&record, lair_priv).await {
        Ok(u)  => u,
        Err(e) => {
            warn!("[proxy] {name}: {e}");
            let _ = mobile_ws.close().await;
            return;
        }
    };
    info!("[proxy] mobile <-> {name} ({url})");
    let (child_ws, _) = match tokio_tungstenite::connect_async(&url).await {
        Ok(p) => p,
        Err(e) => {
            warn!("[proxy] failed to connect to {url}: {e}");
            let _ = mobile_ws.close().await;
            return;
        }
    };
    let (mut mobile_tx, mut mobile_rx) = mobile_ws.split();
    let (mut child_tx, mut child_rx)   = child_ws.split();

    let mobile_to_child = tokio::spawn(async move {
        while let Some(Ok(msg)) = mobile_rx.next().await {
            let forwarded = match msg {
                WsMessage::Text(t)   => child_tx.send(TMessage::Text(t)).await,
                WsMessage::Binary(b) => child_tx.send(TMessage::Binary(b)).await,
                WsMessage::Close(_)  => { let _ = child_tx.send(TMessage::Close(None)).await; break; }
                _ => Ok(()),
            };
            if forwarded.is_err() { break; }
        }
    });

    let child_to_mobile = tokio::spawn(async move {
        while let Some(Ok(msg)) = child_rx.next().await {
            let forwarded = match msg {
                TMessage::Text(t)    => mobile_tx.send(WsMessage::Text(t)).await,
                TMessage::Binary(b)  => mobile_tx.send(WsMessage::Binary(b)).await,
                TMessage::Close(_)   => { let _ = mobile_tx.send(WsMessage::Close(None)).await; break; }
                _ => Ok(()),
            };
            if forwarded.is_err() { break; }
        }
    });

    let _ = tokio::join!(mobile_to_child, child_to_mobile);
    info!("[proxy] mobile <-> {name} closed");
}

// ── System prompt ─────────────────────────────────────────────────────────────

fn build_system_prompt() -> String {
    r#"# Identity & context
You are "lair" — the control-plane agent of an octo deployment. You run as a native OS process on a Linux host machine; child agents are separate OS processes you spawn and supervise locally, OR processes running on remote VMs you provisioned via a cloud-MCP. The user is talking to you over an encrypted Noise tunnel from a mobile client; from here they can chat with you directly, or open a chat with any child (you proxy that chat through yourself — the user never connects to a child directly).

octo can host any kind of agent workload, not only coding agents — don't assume the user is doing software work unless they say so.

# What you help with
1. Orchestration — spin up, tear down, and inspect children, local or remote.
2. Direct work — answer questions, run shell commands, read external resources, and handle small fixes that don't require a child's repo.

# Environment
- Linux host. Lair runs inside a Docker container; local children are plain OS processes (`octo-lair --role agent`) spawned *inside the same container* as lair (non-root uid 10001). Each has a per-agent data dir + workspace under `/data/agents/<name>/` (bind-mounted from `~/.octo/agents/<name>/` on the host) and binds a loopback HTTP port (30100–30199). Mobile reaches a local child via lair's `/agents/<name>/stream` proxy URL.
- Remote children run the same `octo-lair` image on a separate VM you provisioned via a cloud-MCP. The userdata installs Docker, `docker pull`s the lair image, and `docker run`s it with `--role agent` under a systemd unit. They listen on a public Noise port; lair opens an outbound Noise tunnel for the WS proxy so traffic stays encrypted end-to-end. Lair's SSH key bootstraps the VM (drops `config.json` into the host's `/var/lib/octo`, optional repo clone, `systemctl restart` to refresh the container).
- `gh` and `git` are expected to be installed on the host; `GH_TOKEN` is in lair's env when the operator set it via `octo env`.
- MCP servers may be configured at init time or hot-added at runtime; their tools appear alongside the built-ins. `web_fetch` (and `web_search` when Brave is configured) cover external lookups.
- A path prefixed with `@` (e.g. `@core/src/lib.rs`) is a file reference inside a repo — treat it as a path.

# Orchestration tools (lair-specific)
- **`list_agents`** — every known agent (local + remote) with full registry row (name, pid, port, host, pubkey, status, binary_version, instance_id, provider, metadata). Cheap; call before guessing a name.
- **`create_agent`** — args: `git_url?`, `name?`, `port?`, `startup_script?`, `startup_prompt?`. Spawns a new *local* child process.
  - Omit `git_url` for a repo-less workload (default name `lair-workload`); otherwise default name is `lair-<repo-slug>`. `git_url` is a spawn-time argument only — it isn't persisted in the registry. The cloned repo lives in the agent's workspace dir and survives restarts.
  - `port` auto-assigns from 30100–30199 if omitted.
  - `startup_script` runs before the child's HTTP server boots — good for `apt-get`, package installs, git config.
  - `startup_prompt` is sent as the child's first user message once it's ready and triggers a full agentic loop.
  - **Never put secrets in `startup_script` or `startup_prompt`** — provider credentials are forwarded via env automatically; you don't need to bake them in.
- **`mint_bootstrap_userdata`** — args: `name`, `agent_purpose?`, `startup_script?`, `public_port?`, `lair_version?`, `image?`. Returns a cloud-init bash script for a **remote** agent. The userdata is **credentials-free** — it trusts lair's SSH key, installs Docker if absent, `docker pull`s the lair image, and writes a systemd unit that `docker run`s the image with `--role agent`. Hand the returned `userdata` to whichever provisioning MCP the user has configured (AWS, Hetzner, etc.). The MCP returns the new VM's IP → call `register_remote_agent`.
- **`register_remote_agent`** — args: `name`, `host`, `provider?`, `instance_id?`, `git_url?`, `metadata?`. After the provisioning MCP returns the VM's IP, lair SSHes in and: (a) waits for the agent container to publish `/var/lib/octo/lair/agent-info.json` (the agent writes it inside the container; the host sees it via the bind mount), (b) drops `config.json` to `/var/lib/octo/config.json` with the API keys, (c) clones `git_url` into the workspace if given, (d) `systemctl restart`s the agent unit (which `docker run`s a fresh container, picking up the new config). Total timeout ~6 minutes. `name` must match what you passed to `mint_bootstrap_userdata`.
- **`terminate_agent(name)`** — *destructive, local agents only.* Kills the process and deletes the per-agent data + workspace dirs. For remote agents, returns instructions to terminate the VM via the provisioning MCP first, then call `forget_agent`. Always run `list_agents` first to confirm the exact name; confirm with the user before calling unless the request was unambiguous.
- **`forget_agent(name)`** — *registry-only.* Removes a remote agent's row without touching the VM. Use after the provisioning MCP has terminated the instance. Don't use on a live local agent — use `terminate_agent` instead.
- **`restart_all_agents`** — restart every managed *local* agent. Use after upgrading the lair binary; no effect on remote agents.
- **`run_command_in_background(command)`** — run a shell command in the background. The user is notified when it finishes. For long builds, big test suites, large downloads. Prefer the regular `bash` tool for anything fast. When it completes, the output is injected into this conversation autonomously — if no follow-up action is genuinely useful, reply with one short acknowledgement line rather than producing prose.

# General tools (shared with children)
- `bash` — shell commands; use for git, gh, curl, one-offs.
- `read_file(path, offset?, limit?)` — pair with `grep` first; never read a whole file just to skim.
- `grep(pattern, path?, context?)` — returns `file:line` you can feed back into `read_file`.
- `glob(pattern)` — file-path search. Anchor from a known root; never start a path argument with `**`.
- `edit_file(path, old_str, new_str)` — exact string replace; `old_str` must match exactly once. Prefer over `write_file` on existing files.
- `write_file(path, content)` — new files only.

# Working with children
- You orchestrate children; you do **not** message them. If the user asks "have child X do Y", tell them to open the child's own chat in the mobile app — that's the direct path (it proxies through you transparently). You can still answer cluster-wide questions about the child (status, port, host) from `list_agents`.

# Local vs remote agents
- **Local**: `create_agent` → OS process on this host. Reachable via loopback. Default when the user doesn't mention a cloud / instance type.
- **Remote**: a 3-step LLM-driven flow that uses the user's configured cloud MCP. Userdata carries no credentials — lair finishes bootstrap over SSH.
  1. `mint_bootstrap_userdata(name=…, agent_purpose?=…)` — get the credentials-free userdata.
  2. Call the provisioning MCP with that userdata verbatim, plus user-specified region / instance_type / security group. The MCP returns a public IP + instance id.
  3. `register_remote_agent(name=…, host=<public_ip>, git_url?=…, provider=…, instance_id=…)` — lair SSHes in, finishes the bootstrap, and registers the row.

## CRITICAL: where `host` and `instance_id` come from
- Take **both** `host` (the public IP) and `instance_id` **only from the provisioning MCP's `run-instances` (or equivalent) tool result** — the one whose payload includes a clear `Instances[*].InstanceId` and `PublicIpAddress`. That tool returns them as structured JSON; copy them verbatim.
- **Do not** derive `host` or `instance_id` from:
  - a `bash` call to `aws ec2 describe-instances` or similar (it can surface stale instances from earlier failed attempts);
  - your own memory of an earlier turn;
  - the *first* `run-instances` attempt if you had to retry the call with corrected args (use the IP from the **successful** call, not the failed ones);
  - any non-MCP source.
- If the provisioning MCP returned more than one instance or its response is ambiguous, re-call the MCP with `--query` / equivalent to disambiguate. Do **not** guess.
- Before calling `register_remote_agent`, you may sanity-check the IP with the AWS MCP (e.g. `describe-instances --instance-ids <id> --query 'Reservations[].Instances[].PublicIpAddress'`). Do not use `bash` for this — the MCP's structured response is the source of truth.

## SG / firewall requirements before `register_remote_agent`
- The instance's **security group must allow inbound TCP 22 from lair's host** (lair drives the SSH bootstrap), plus the agent's Noise port (default 9000) from the same source. If the user didn't pre-create an SG with these rules, do so before launching, or ask the MCP to attach one that does.
- The instance must be in a **public subnet** — i.e. one whose route table has `0.0.0.0/0` pointing at an Internet Gateway. A "public IP" assigned in a non-public subnet is non-functional.

## Other remote-agent notes
- Lair's SSH keypair is at `<OCTO_DATA_DIR>/ssh_id_ed25519`. `mint_bootstrap_userdata` always embeds the matching pubkey in the userdata it returns.
- Termination: for remote agents, `terminate_agent` returns instructions — call the provisioning MCP's terminate-instance method (using `instance_id` from `list_agents`), then `forget_agent(name)`.
- Trigger the remote flow when the user names a cloud / instance type / region, OR when they ask for hardware lair doesn't have locally (GPUs, etc.).

# Response style
- Concise and direct; the user is often on a phone screen.
- Don't narrate tool calls ("Let me check…", "I'll now…", "I've completed…").
- Don't summarize tool output back to the user — they can see it. Write prose only for real answers, questions, or recommendations.
- No filler openers ("Sure!", "Of course!", "Great question!").
- When you call a tool, call it — don't announce it first.

# Safety
- Never commit or push git changes unless the user explicitly asked.
- Confirm before `terminate_agent` or `restart_all_agents` unless the user just told you to.
- If a request would put a secret into plaintext config (`startup_script`, `startup_prompt`, env), flag it and offer a safer alternative.
- Trust your judgment on small choices; only ask when ambiguity would actually change the outcome."#
        .to_string()
}

// ── Tools ─────────────────────────────────────────────────────────────────────

fn create_agent_tool() -> AnthropicTool {
    AnthropicTool {
        name: "create_agent".to_string(),
        description: "Spawn a new *local* octo child agent as an OS process on the lair host. \
                       Handles per-agent dir layout (~/.octo/agents/<name>/{data,workspace}/) \
                       and loopback port assignment (30100–30199). For remote agents on a \
                       cloud VM, use mint_bootstrap_userdata + register_remote_agent instead."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "git_url": {
                    "type": "string",
                    "description": "Git repository URL to clone into the agent's workspace at spawn time. Not stored in the registry — the cloned repo lives in the workspace dir and survives restarts."
                },
                "name": {
                    "type": "string",
                    "description": "Optional name override. Defaults to lair-<repo-name>, or lair-workload if no git_url."
                },
                "port": {
                    "type": "integer",
                    "description": "Optional loopback port (30100–30199). Auto-assigned if omitted."
                },
                "startup_script": {
                    "type": "string",
                    "description": "Optional shell script run inside the child before its HTTP server starts. Never include sensitive data — these are stored as plaintext env on the process."
                },
                "startup_prompt": {
                    "type": "string",
                    "description": "Optional initial prompt sent to the child's agentic loop once ready. Never include sensitive data."
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
        description: "Permanently terminate a child agent: kill the process and \
                       delete its per-agent data + workspace directories. Irreversible."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Name of the child to terminate." }
            },
            "required": ["name"]
        }),
        display_label: Some("Terminating agent".into()),
    }
}

fn list_agents_tool() -> AnthropicTool {
    AnthropicTool {
        name: "list_agents".to_string(),
        description: "List every known agent — local + remote — with the full registry row \
                       (name, pid, port, host, pubkey, status, binary_version, instance_id, \
                       provider, metadata). Cheap; call before guessing a name."
            .to_string(),
        input_schema: serde_json::json!({"type":"object","properties":{},"required":[]}),
        display_label: Some("Listing agents".into()),
    }
}

fn restart_all_agents_tool() -> AnthropicTool {
    AnthropicTool {
        name: "restart_all_agents".to_string(),
        description: "Stop and respawn every managed local agent. Use after upgrading the lair \
                       binary. No effect on remote agents.".to_string(),
        input_schema: serde_json::json!({"type":"object","properties":{},"required":[]}),
        display_label: Some("Restarting agents".into()),
    }
}

fn mint_bootstrap_userdata_tool() -> AnthropicTool {
    AnthropicTool {
        name: "mint_bootstrap_userdata".to_string(),
        description: "Mint a cloud-init bash script (\"userdata\") for bootstrapping a remote \
                       octo agent on a freshly-provisioned Linux VM. The userdata contains **no \
                       credentials** — only lair's SSH public key, a Docker install (if absent), \
                       a `docker pull` of the multi-arch `octo-lair` image, and a systemd unit \
                       that `docker run`s the image with `--role agent`. The agent boots without \
                       API keys; lair finishes the bootstrap over SSH afterwards (drops \
                       config.json into the container's bind-mounted /data, optionally clones \
                       the git repo, and `systemctl restart`s the unit — which restarts the \
                       container, picking up the fresh config). Returns the userdata blob plus \
                       the agent name. After the provisioning MCP returns the new VM's IP, \
                       call `register_remote_agent(name=…, host=<public_ip>, ...)`."
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
                    "description": "One-line mission baked into the agent's system prompt (used only if no git_url is later supplied to register_remote_agent)."
                },
                "startup_script": {
                    "type": "string",
                    "description": "Optional bash run inside the agent process at boot, before its HTTP server binds. Will not have access to API keys (they arrive later via SSH); use for package installs."
                },
                "public_port": {
                    "type": "integer",
                    "description": "Public TCP port the agent's Noise endpoint listens on (default 9000). Security group must allow inbound TCP on this port plus SSH (22) from lair's IP."
                },
                "lair_version": {
                    "type": "string",
                    "description": "Lair image tag to pull (without leading v). Defaults to lair's own running version. Only used to compose the default `image` — overridden by `image` if both are passed."
                },
                "image": {
                    "type": "string",
                    "description": "Explicit lair image reference (e.g. `ghcr.io/you/octo-lair:0.10.1`). Defaults to `ghcr.io/georgebradford0/octo-lair:<lair_version>`."
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
                       `/var/lib/octo/lair/agent-info.json` (host path; bind-mounted to \
                       `/data/lair/` inside the agent container), drops `config.json` to \
                       `/var/lib/octo/config.json` with the API keys, optionally clones \
                       `git_url` into the workspace, and `systemctl restart`s the agent service \
                       (which restarts the docker container). Total timeout ~6 minutes. `name` \
                       must match what was passed to `mint_bootstrap_userdata`. Each SSH op \
                       retries internally with exponential backoff. A `Pending` registry row \
                       is inserted as soon as the agent's identity is known."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Logical agent name — must match mint_bootstrap_userdata." },
                "host": { "type": "string", "description": "Public IP or DNS name of the VM (from the provisioning MCP's response)." },
                "provider":    { "type": "string", "description": "Free-form provider tag (e.g. aws, gcp, hetzner)." },
                "instance_id": { "type": "string", "description": "Cloud instance id (e.g. i-0abc...)." },
                "git_url":     { "type": "string", "description": "Optional Git URL to clone into the agent's workspace after the process is up. Lair uses its own GH_TOKEN for HTTPS clones." },
                "metadata":    { "type": "object", "description": "Opaque provider-specific blob (region, instance_type, image id, ...). Stored alongside the row." }
            },
            "required": ["name", "host"]
        }),
        display_label: Some("Registering remote agent".into()),
    }
}

fn forget_agent_tool() -> AnthropicTool {
    AnthropicTool {
        name: "forget_agent".to_string(),
        description: "Remove an agent's registry row without touching processes or any VM. Use \
                       after the provisioning MCP has terminated a remote instance, to clean up \
                       the dangling row. Don't use on a live local agent — use `terminate_agent` \
                       instead."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Agent name to forget." }
            },
            "required": ["name"]
        }),
        display_label: Some("Forgetting agent".into()),
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
        restart_all_agents_tool(),
        run_command_in_background_tool(),
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
                "list_agents"               => exec_list_agents(state.clone()).await,
                "create_agent"              => exec_create_agent(state, input).await,
                "mint_bootstrap_userdata"   => exec_mint_bootstrap_userdata(state, input).await,
                "register_remote_agent"     => exec_register_remote_agent(state, input).await,
                "terminate_agent"           => exec_terminate_agent(state, input).await,
                "forget_agent"              => exec_forget_agent(state, input).await,
                "restart_all_agents"        => exec_restart_all_agents(state).await,
                "run_command_in_background" => exec_run_command_in_background(state, input).await,
                other => format!("unknown tool: {other}"),
            }
        })
    }))
}

async fn exec_list_agents(state: Arc<AppState>) -> String {
    let records = state.registry.lock().unwrap().list().to_vec();
    serde_json::to_string_pretty(&records).unwrap_or_else(|e| format!("error: {e}"))
}

async fn exec_create_agent(state: Arc<AppState>, input: serde_json::Value) -> String {
    let git_url = input.get("git_url").and_then(|v| v.as_str()).map(str::to_string);

    let child_name = input.get("name").and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| match &git_url {
            Some(u) => {
                let slug = u.trim_end_matches('/')
                    .split('/').last().unwrap_or("repo")
                    .trim_end_matches(".git").to_lowercase();
                format!("lair-{slug}")
            }
            None => "lair-workload".to_string(),
        });

    if state.registry.lock().unwrap().get(&child_name).is_some() {
        return format!("error: agent '{child_name}' already exists");
    }

    let startup_script = input.get("startup_script").and_then(|v| v.as_str()).map(str::to_string);
    let startup_prompt = input.get("startup_prompt").and_then(|v| v.as_str()).map(str::to_string);

    let port: u16 = match input.get("port")
        .or_else(|| input.get("noise_port")) // accept legacy name
        .and_then(|v| v.as_u64())
    {
        Some(p) => p as u16,
        None => match state.registry.lock().unwrap().assign_free_port(30100..=30199) {
            Some(p) => p,
            None    => return "error: no free loopback ports in 30100–30199".to_string(),
        },
    };

    info!("[lair/create_agent] creating {child_name} port={port} git={}", git_url.as_deref().unwrap_or("(none)"));

    let cfg = octo_core::read_config();
    let gh_token = std::env::var("GH_TOKEN").ok().filter(|s| !s.is_empty());

    let params = SpawnParams {
        name:              &child_name,
        port,
        git_url:           git_url.as_deref(),
        startup_script:    startup_script.as_deref(),
        startup_prompt:    startup_prompt.as_deref(),
        anthropic_api_key: cfg.anthropic_api_key.as_deref(),
        openai_api_key:    cfg.openai_api_key.as_deref(),
        openai_api_url:    cfg.api_url.as_deref(),
        model:             cfg.model.as_deref(),
        gh_token:          gh_token.as_deref(),
        agent_purpose:     None,
    };

    match state.supervisor.spawn(&params).await {
        Ok(pid) => {
            let now = octo_core::now_secs();
            let record = AgentRecord {
                name:           child_name.clone(),
                pid:            Some(pid),
                port,
                host:           None,
                pubkey:         None,
                status:         AgentStatus::Pending,
                binary_version: env!("CARGO_PKG_VERSION").to_string(),
                created_at:     now,
                last_seen:      now,
                instance_id:    None,
                provider:       None,
                metadata:       serde_json::Value::Null,
            };
            let add_result = state.registry.lock().unwrap().add(record);
            if let Err(e) = add_result {
                error!("[lair/create_agent] registry add failed: {e:#}");
                let _ = state.supervisor.stop(&child_name).await;
                return format!("error registering '{child_name}': {e:#}");
            }
            info!("[lair/create_agent] created {child_name} pid={pid}");
            state.poll_trigger.notify_one();
            format!("Created child '{child_name}' (pid {pid}) on loopback port {port}.")
        }
        Err(e) => {
            error!("[lair/create_agent] failed: {e:#}");
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
    let lair_version   = input.get("lair_version")  .and_then(|v| v.as_str()).map(str::to_string)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let image = input.get("image").and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("ghcr.io/georgebradford0/octo-lair:{lair_version}"));

    let lair_pubkey = match ssh_ops::read_lair_public_key() {
        Ok(k) => k,
        Err(e) => return format!("error reading lair SSH public key: {e:#}"),
    };

    // Env file passed to `docker run --env-file`. All container-internal
    // paths — the image bakes `OCTO_HOME=/data` and `OCTO_DATA_DIR=/data/lair`
    // already, and `-v /var/lib/octo:/data` maps those to host paths.
    let mut env_lines: Vec<String> = vec![
        "AGENT_PORT=8000".to_string(),
        format!("AGENT_NOISE_PORT={public_port}"),
        "WORKSPACE_DIR=/data/workspace".to_string(),
        "OCTO_SKIP_SHELL_ENV=1".to_string(),
    ];
    if let Some(v) = &agent_purpose  { env_lines.push(format!("AGENT_PURPOSE={v}")); }
    if let Some(v) = &startup_script { env_lines.push(format!("STARTUP_SCRIPT={v}")); }
    let env_content = env_lines.join("\n");

    let userdata = format!(r#"#!/bin/bash
set -eux

# 1. Trust lair's operator SSH key.
mkdir -p /root/.ssh && chmod 700 /root/.ssh
cat >> /root/.ssh/authorized_keys <<'OCTO_SSH_EOF'
{lair_pubkey}
OCTO_SSH_EOF
chmod 600 /root/.ssh/authorized_keys

# 2. Install Docker if it isn't already there. Uses the official Docker
#    convenience script, which handles apt/yum/apk distros transparently.
if ! command -v docker >/dev/null 2>&1; then
    curl -fsSL https://get.docker.com | sh
fi
systemctl enable --now docker

# 3. Host dirs that the agent container bind-mounts at /data. Lair's SSH
#    bootstrap phase writes `/var/lib/octo/config.json` (the operator's
#    API keys) and reads `/var/lib/octo/lair/agent-info.json` (the agent's
#    Noise pubkey + port) — both via this same bind mount.
install -d -m 700 /var/lib/octo /var/lib/octo/lair /var/lib/octo/workspace /etc/octo

# 4. Non-secret bootstrap env passed to `docker run --env-file`. API keys
#    are dropped over SSH after the container is up; the container is
#    restarted afterwards via `systemctl restart octo-agent`.
umask 077
cat > /etc/octo/agent.env <<'OCTO_ENV_EOF'
{env_content}
OCTO_ENV_EOF
umask 022

# 5. Pull the multi-arch lair image. The image hosts both --role lair and
#    --role agent; we override the entrypoint below to pick the agent role.
docker pull "{image}"

# 6. systemd unit drives `docker run`. ExecStartPre removes any stale
#    container; ExecStart launches a fresh one in the foreground so systemd
#    can supervise it. Restart is always — if the container crashes or the
#    host reboots, systemd brings it back.
cat > /etc/systemd/system/octo-agent.service <<'OCTO_UNIT_EOF'
[Unit]
Description=octo agent container
Requires=docker.service
After=docker.service network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStartPre=-/usr/bin/docker rm -f octo-agent
ExecStart=/usr/bin/docker run --rm --name octo-agent \
    -p {public_port}:{public_port} \
    -v /var/lib/octo:/data \
    --env-file /etc/octo/agent.env \
    --entrypoint /usr/local/bin/octo-lair \
    {image} --role agent
ExecStop=/usr/bin/docker stop -t 10 octo-agent
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
OCTO_UNIT_EOF

systemctl daemon-reload
systemctl enable --now octo-agent
"#);

    let result = serde_json::json!({
        "name":     name,
        "userdata": userdata,
        "lair_version": lair_version,
        "instructions": format!(
            "Hand `userdata` to the provisioning MCP as the new instance's user-data. \
             Make sure the security group / firewall allows inbound TCP {public_port} (for lair's \
             Noise tunnel) and 22 (for lair's SSH-driven bootstrap). The userdata contains no \
             credentials. After the MCP returns the public IP, call \
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
            "error: lair has no SSH private key at {}. Restart lair to generate one.",
            key_path.display(),
        );
    }

    // Resumption logic. Any `Pending` row for this name is treated as
    // retry-able — including ones that recorded a different `host` from a
    // bad earlier attempt (e.g. the LLM passing the wrong IP). We overwrite
    // the prior host with the new one in the Pending row we drop below.
    // `Running` rows error out — the caller has to `forget_agent` (and
    // terminate the cloud instance) first to avoid clobbering a working
    // remote agent.
    let prior = state.registry.lock().unwrap().get(&name).cloned();
    let (created_at, resuming) = match prior {
        Some(r) if matches!(r.status, AgentStatus::Pending) => {
            if r.host.as_deref() == Some(host.as_str()) {
                info!("[lair/register_remote_agent] resuming pending registration of '{name}' at {host}");
                (r.created_at, true)
            } else {
                warn!(
                    "[lair/register_remote_agent] overwriting pending registration of '{name}' \
                     (was at host={:?}, now {host}) — prior attempt likely used a wrong IP",
                    r.host,
                );
                (r.created_at, false)
            }
        }
        Some(r) => {
            return format!(
                "error: agent '{name}' is already in the registry (status={}, host={:?}). \
                 To re-register against a different host, run `forget_agent('{name}')` first \
                 (and terminate the prior cloud instance via the provisioning MCP if it's still up).",
                r.status.as_wire_str(),
                r.host,
            );
        }
        None => (octo_core::now_secs(), false),
    };

    // Pre-flight: TCP probe `host:22` before kicking off the multi-minute
    // SSH wait loop. Catches the common "wrong IP" / "bad SG" / "subnet has
    // no IGW route" cases in ~30 s with a useful error, instead of 5 min of
    // opaque "Connection timed out" log spam. We retry for 30 s so a VM
    // that's still in the very early stages of cloud-init (sshd not bound
    // yet) doesn't get false-positived as unreachable.
    info!("[lair/register_remote_agent] {host}: pre-flight TCP probe on port 22");
    if let Err(e) = probe_tcp_reachable(&host, 22, Duration::from_secs(30)).await {
        return format!(
            "error: {host}:22 is not reachable from lair after 30s ({e}).\n\
             \n\
             Common causes — verify each:\n\
             1. **Wrong IP.** Take `host` only from the provisioning MCP's \
                run-instances (or equivalent) response. Don't derive it from a \
                `bash` `describe-instances` call, your memory, or an earlier \
                failed attempt — those can yield a stale or hallucinated id. \
                If unsure, re-call the MCP and confirm the IP against the \
                actual `i-…` instance id you intend to use.\n\
             2. **Security group.** The instance's SG must allow inbound TCP \
                22 (and your `public_port`, default 9000) from lair's IP.\n\
             3. **Subnet routing.** The subnet's route table must route \
                0.0.0.0/0 to an Internet Gateway. A 'public IP' assigned in \
                a subnet without IGW routing is non-functional.\n\
             4. **Instance state.** Verify the instance is `running` (not \
                `pending`, `shutting-down`, or terminated)."
        );
    }

    // Phase 1: wait for the agent to publish agent-info.json.
    info!("[lair/register_remote_agent] {host}: waiting for agent-info.json");
    let info = match ssh_ops::await_agent_info(
        &host, "root", &key_path,
        Duration::from_secs(300),
        Duration::from_secs(8),
    ).await {
        Ok(i) => i,
        Err(e) => return format!("error: could not pull agent info from {host}: {e:#}"),
    };

    // Insert / refresh Pending row.
    {
        let pending = AgentRecord {
            name:           name.clone(),
            pid:            None,
            port:           info.port,
            host:           Some(host.clone()),
            pubkey:         Some(info.pubkey.clone()),
            status:         AgentStatus::Pending,
            binary_version: env!("CARGO_PKG_VERSION").to_string(),
            created_at,
            last_seen:      octo_core::now_secs(),
            instance_id:    instance_id.clone(),
            provider:       provider.clone(),
            metadata:       metadata.clone(),
        };
        if let Err(e) = state.registry.lock().unwrap().set(pending) {
            return format!("error inserting pending registry row: {e:#}");
        }
        state.poll_trigger.notify_one();
    }

    // Phase 2: drop config.json over SSH.
    let lair_cfg = octo_core::read_config();
    let cfg = serde_json::json!({
        "name":              null,
        "anthropic_api_key": lair_cfg.anthropic_api_key,
        "openai_api_key":    lair_cfg.openai_api_key,
        "model":             lair_cfg.model,
        "api_url":           lair_cfg.api_url,
    });
    let cfg_str = match serde_json::to_string_pretty(&cfg) {
        Ok(s) => s,
        Err(e) => return format!("error encoding config.json: {e:#}"),
    };
    info!("[lair/register_remote_agent] {host}: dropping {}", ssh_ops::REMOTE_CONFIG_PATH);
    if let Err(e) = ssh_ops::write_file(
        &host, "root", &key_path,
        ssh_ops::REMOTE_CONFIG_PATH,
        &cfg_str,
        0o600,
    ).await {
        return format!(
            "error writing config.json to {host}: {e:#}. Re-run register_remote_agent to retry.",
        );
    }

    // Phase 3: optional git clone.
    if let Some(url) = git_url.clone() {
        let token = std::env::var("GH_TOKEN").unwrap_or_default();
        let user_name  = std::env::var("GIT_USER_NAME") .unwrap_or_else(|_| "octo".to_string());
        let user_email = std::env::var("GIT_USER_EMAIL").unwrap_or_else(|_| "octo@localhost".to_string());
        let script = build_remote_clone_script(&url, &token, &user_name, &user_email);
        info!("[lair/register_remote_agent] {host}: cloning {url}");
        if let Err(e) = ssh_ops::run_script(&host, "root", &key_path, &script).await {
            return format!(
                "error cloning git repo on {host}: {e:#}. Re-run register_remote_agent to retry.",
            );
        }
    }

    // Phase 4: systemctl restart to pick up config + cloned workspace.
    info!("[lair/register_remote_agent] {host}: restarting octo-agent");
    if let Err(e) = ssh_ops::run_script(
        &host, "root", &key_path,
        "set -e; systemctl restart octo-agent",
    ).await {
        return format!(
            "error restarting octo-agent on {host}: {e:#}. Re-run register_remote_agent to retry.",
        );
    }

    // Phase 5: confirm the restart by re-reading agent-info.json (soft fail).
    info!("[lair/register_remote_agent] {host}: confirming restart");
    let info = match ssh_ops::await_agent_info(
        &host, "root", &key_path,
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
        name:           name.clone(),
        pid:            None,
        port:           info.port,
        host:           Some(host.clone()),
        pubkey:         Some(info.pubkey.clone()),
        status:         AgentStatus::Running,
        binary_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at,
        last_seen:      octo_core::now_secs(),
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
         Mobile will pick it up via the next agents event.",
        info.port, info.pubkey,
    )
}

fn build_remote_clone_script(
    url:        &str,
    gh_token:   &str,
    user_name:  &str,
    user_email: &str,
) -> String {
    let clone_url = if url.starts_with("https://") && !gh_token.is_empty() {
        let rest = url.trim_start_matches("https://");
        let rest = match rest.find('@') { Some(i) => &rest[i + 1..], None => rest };
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
    script.push_str(&format!("WORKSPACE={}\n", ssh_ops::REMOTE_WORKSPACE_PATH));
    script.push_str(&format!("CLONE_URL='{}'\n", clone_url.replace('\'', "'\\''")));
    script.push_str(&format!("USER_NAME='{}'\n",  user_name.replace('\'', "'\\''")));
    script.push_str(&format!("USER_EMAIL='{}'\n", user_email.replace('\'', "'\\''")));
    script.push_str(r#"if [ -d "$WORKSPACE/.git" ]; then
    git -C "$WORKSPACE" remote set-url origin "$CLONE_URL"
    git -C "$WORKSPACE" fetch --all
else
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
            "error: '{name}' is a local agent. Use `terminate_agent` instead so its process \
             and per-agent data dir are cleaned up."
        );
    }

    let removed = state.registry.lock().unwrap().remove(&name);
    match removed {
        Ok(true) => {
            state.poll_trigger.notify_one();
            format!("Forgot '{name}' — registry row removed; no VM action taken.")
        }
        Ok(false) => format!("'{name}' was not in the registry"),
        Err(e)    => format!("error: {e:#}"),
    }
}

async fn exec_terminate_agent(state: Arc<AppState>, input: serde_json::Value) -> String {
    let name = match input.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None    => return "error: missing 'name' field".to_string(),
    };
    match terminate_agent_by_name(&state, &name).await {
        Ok(_)  => format!("Terminated '{name}' and removed its data + workspace directories."),
        Err(e) => format!("error: {e}"),
    }
}

async fn exec_restart_all_agents(state: Arc<AppState>) -> String {
    // Skip remote agents — they run on VMs lair doesn't supervise directly.
    let names: Vec<String> = state.registry.lock().unwrap()
        .list().iter().filter(|r| !r.is_remote()).map(|r| r.name.clone()).collect();
    if names.is_empty() {
        info!("[lair/restart_all] no local agents found");
        return "No local agents to restart.".to_string();
    }
    let mut restarted = Vec::new();
    for name in &names {
        if let Err(e) = state.supervisor.stop(name).await {
            warn!("[lair/restart_all] stop {name}: {e:#}");
        }
        if let Err(e) = start_agent_by_name(&state, name).await {
            error!("[lair/restart_all] start {name}: {e}");
        } else {
            restarted.push(name.clone());
        }
    }
    state.poll_trigger.notify_one();
    info!("[lair/restart_all] restarted: {}", restarted.join(", "));
    format!("Restarted: {}.", restarted.join(", "))
}

async fn exec_run_command_in_background(state: Arc<AppState>, input: serde_json::Value) -> String {
    let command = match input.get("command").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return "error: missing or empty 'command'".to_string(),
    };

    let task_id = format!("bg-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    info!("[lair/run_command_in_background] spawning {task_id} ({} chars)", command.len());

    let cancel = CancellationToken::new();
    register_task(&state.stream_state, &data_dir(), TaskRecord {
        task_id:      task_id.clone(),
        command:      command.clone(),
        status:       TaskStatus::Running,
        started_at:   now_secs(),
        completed_at: None,
        summary:      None,
        cost_usd:     None,
    }, cancel.clone());
    buffer_and_fanout(&state.stream_state, tasks_wire_json(&state.stream_state));

    let params = BackgroundCommandParams {
        task_id: task_id.clone(),
        command,
        cwd:     "/".to_string(),
    };

    let progress_state   = state.clone();
    let progress_task_id = task_id.clone();
    let progress = move |output_tail: &str| {
        record_task_progress(&progress_state.stream_state, &progress_task_id, output_tail);
        buffer_and_fanout(&progress_state.stream_state, tasks_wire_json(&progress_state.stream_state));
    };

    let stream_state_arc = state.clone();
    spawn_background_command(params, cancel, progress, move |outcome| {
        finalize_task(&stream_state_arc.stream_state, &data_dir(), &outcome);
        buffer_and_fanout(&stream_state_arc.stream_state, tasks_wire_json(&stream_state_arc.stream_state));
        let injection = format!(
            "Background command {} completed (status={}). Command: {}\n\nOutput:\n{}",
            outcome.task_id, outcome.status, outcome.command, outcome.summary
        );
        {
            let mut msgs = stream_state_arc.messages.lock().unwrap();
            msgs.push(ApiMessage {
                role:    "bg_complete".to_string(),
                content: vec![ContentBlock::Text { text: injection.clone() }],
            });
            save_messages(&msgs);
        }
        let bg_event = ChatEvent::BgComplete {
            task_id: outcome.task_id.clone(),
            text:    injection,
        };
        if let Some(json) = chat_event_to_wire_json(&bg_event) {
            buffer_and_fanout(&stream_state_arc.stream_state, json.to_string());
        }
        let event = completion_chat_event(&outcome);
        if let Some(json) = chat_event_to_wire_json(&event) {
            buffer_and_fanout(&stream_state_arc.stream_state, json.to_string());
        }
        let signer = stream_state_arc.relay_signer.clone();
        let url    = stream_state_arc.relay_url.clone();
        if !url.is_empty() {
            let title = format!("Background command {}", outcome.status);
            let body  = outcome.summary.chars().take(120).collect::<String>();
            tokio::spawn(async move {
                relay_client::notify(&url, &signer, "task_complete", Some(&title), Some(&body)).await;
            });
        }
        try_continue_auto(stream_state_arc.clone());
    });

    format!("Background command {task_id} started. The user will be notified when it completes.")
}

// ── Management HTTP API (CLI ↔ lair on loopback) ───────────────────────────────

#[derive(Deserialize, Default)]
struct CreateAgentBody {
    name:           Option<String>,
    git_url:        Option<String>,
    port:           Option<u16>,
    startup_script: Option<String>,
    startup_prompt: Option<String>,
}

async fn cli_list_agents(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let records = state.registry.lock().unwrap().list().to_vec();
    Json(serde_json::to_value(&records).unwrap_or(serde_json::Value::Array(vec![])))
}

async fn cli_create_agent(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<CreateAgentBody>,
) -> Response {
    let mut input = serde_json::Map::new();
    if let Some(v) = body.name           { input.insert("name".into(),           serde_json::Value::String(v)); }
    if let Some(v) = body.git_url        { input.insert("git_url".into(),        serde_json::Value::String(v)); }
    if let Some(v) = body.port           { input.insert("port".into(),           serde_json::Value::Number(v.into())); }
    if let Some(v) = body.startup_script { input.insert("startup_script".into(), serde_json::Value::String(v)); }
    if let Some(v) = body.startup_prompt { input.insert("startup_prompt".into(), serde_json::Value::String(v)); }
    let out = exec_create_agent(state, serde_json::Value::Object(input)).await;
    if out.starts_with("error") {
        (StatusCode::BAD_REQUEST, out).into_response()
    } else {
        (StatusCode::OK, out).into_response()
    }
}

async fn cli_start_agent(
    AxumPath(name): AxumPath<String>,
    State(state):   State<Arc<AppState>>,
) -> Response {
    match start_agent_by_name(&state, &name).await {
        Ok(_)  => (StatusCode::OK, "ok").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn cli_stop_agent(
    AxumPath(name): AxumPath<String>,
    State(state):   State<Arc<AppState>>,
) -> Response {
    let exists = state.registry.lock().unwrap().get(&name).is_some();
    if !exists {
        return (StatusCode::NOT_FOUND, format!("agent '{name}' not found")).into_response();
    }
    match state.supervisor.stop(&name).await {
        Ok(_) => {
            {
                let mut reg = state.registry.lock().unwrap();
                let _ = reg.update_pid(&name, None);
                let _ = reg.update_status(&name, AgentStatus::Stopped);
            }
            state.poll_trigger.notify_one();
            (StatusCode::OK, "ok").into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn cli_delete_agent(
    AxumPath(name): AxumPath<String>,
    State(state):   State<Arc<AppState>>,
) -> Response {
    match terminate_agent_by_name(&state, &name).await {
        Ok(_)  => (StatusCode::OK, "ok").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn cli_agent_logs(
    AxumPath(name): AxumPath<String>,
    State(state):   State<Arc<AppState>>,
) -> Response {
    let exists = state.registry.lock().unwrap().get(&name).is_some();
    if !exists {
        return (StatusCode::NOT_FOUND, format!("agent '{name}' not found")).into_response();
    }
    match state.supervisor.log_tail(&name, 1024 * 1024) {
        Ok(s)  => (StatusCode::OK, s).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
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

    let dir = data_dir();
    fs::create_dir_all(&dir).ok();

    let is_dev   = std::env::var("OCTO_DEV").as_deref() == Ok("1");
    let key_file = std::env::var("NOISE_KEY_FILE")
        .unwrap_or_else(|_| dir.join("noise_key.bin").to_string_lossy().to_string());

    if print_pubkey {
        let pubkey = if is_dev {
            DEV_PUBKEY_BASE32.to_string()
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
    } else {
        load_or_generate_keypair(&key_file)
    };

    let pubkey_b32 = to_base32(&static_public);
    let noise_port: u16  = std::env::var("NOISE_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(9000);
    let public_port: u16 = std::env::var("PUBLIC_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(noise_port);
    let http_port:  u16  = 8000;
    let public_host = crate::bootstrap::resolve_public_host("lair").await?;
    crate::bootstrap::run_startup_script("lair").await?;

    info!("[lair] noise_pubkey={pubkey_b32} noise_port={noise_port} http_port={http_port} public_host={public_host}");

    // Keep a clone of the private key around so AppState can use it as the
    // Noise *initiator* key when proxying mobile traffic to remote agents.
    let lair_priv = static_private.clone();
    tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

    // Operator SSH key — generated once for ops use (e.g. SSHing into hosts);
    // kept even though the remote-agent flow was removed, in case the user
    // wants to use it for unrelated ops.
    match ensure_ssh_keypair(&dir) {
        Ok((priv_path, _pub_path)) => info!("[lair] SSH keypair ready at {}", priv_path.display()),
        Err(e) => warn!("[lair] could not ensure SSH keypair: {e:#}"),
    }

    // Agents root: `<OCTO_DATA_DIR>/../agents` so multiple lairs on one host
    // wouldn't share dirs. Default operator layout has it at `~/.octo/agents`.
    let agents_root = std::env::var("OCTO_AGENTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            // Sibling of the lair data dir by default.
            dir.parent().map(|p| p.join("agents")).unwrap_or_else(|| dir.join("agents"))
        });
    fs::create_dir_all(&agents_root).ok();
    info!("[lair] agents_root = {}", agents_root.display());
    let supervisor = AgentSupervisor::new(agents_root.clone())
        .map_err(|e| anyhow::anyhow!("init supervisor: {e:#}"))?;

    let registry = Registry::load(dir.join("agents.json"))
        .map_err(|e| anyhow::anyhow!("load agent registry: {e:#}"))?;

    // Re-adopt any children whose recorded pid is still alive after a lair
    // restart, and clear pid on rows whose process is gone (so the poller
    // surfaces them as Stopped).
    {
        let mut adopted = 0usize;
        let mut cleared = 0usize;
        let snapshot: Vec<AgentRecord> = registry.list().to_vec();
        let mut reg_inner = registry; // shadow so we can mutate via &mut
        for record in snapshot {
            if let Some(pid) = record.pid {
                if AgentSupervisor::is_alive(pid) {
                    supervisor.adopt(&record.name, pid);
                    adopted += 1;
                } else {
                    let _ = reg_inner.update_pid(&record.name, None);
                    let _ = reg_inner.update_status(&record.name, AgentStatus::Stopped);
                    cleared += 1;
                }
            }
        }
        info!("[lair] registry init: {} agent(s); adopted={adopted} cleared={cleared}", reg_inner.list().len());
        let registry = Arc::new(Mutex::new(reg_inner));

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
        let (agents_tx, agents_rx) = watch::channel(Vec::<AgentWire>::new());
        let (ready_tx, ready_rx)   = watch::channel(false);

        let relay_signer  = Arc::new(RelaySigner::load_or_generate(
            &dir.join(RELAY_SIGNING_KEY_FILE).to_string_lossy(),
        ));
        let relay_url_str = std::env::var("OCTO_RELAY_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_RELAY_URL.to_string());
        info!("[lair] relay_signing_pubkey={} relay_url={}", relay_signer.pubkey_b32(), relay_url_str);

        // Management API token — read once at startup, then removed from
        // the in-memory env. `/proc/1/environ` still holds the originally
        // exec'd value, but children run as a different uid (see
        // `agent_proc::spawn`) so they can't read it.
        let mgmt_token = std::env::var("LAIR_MGMT_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        std::env::remove_var("LAIR_MGMT_TOKEN");
        if mgmt_token.is_some() {
            info!("[lair] management API gated on X-Octo-Token header");
        } else {
            warn!("[lair] LAIR_MGMT_TOKEN not set — management endpoints (POST /agents, /:name/start, /:name/stop, DELETE /:name) are OPEN to peer processes inside the container. Production deploys should always set this.");
        }

        let state = Arc::new(AppState {
            messages:      Arc::new(Mutex::new(messages)),
            last_cost_usd: Mutex::new(None),
            system:        build_system_prompt(),
            agents_tx,
            agents_rx,
            poll_trigger:  poll_trigger.clone(),
            pubkey_b32:    pubkey_b32.clone(),
            public_host:   public_host.clone(),
            lair_priv,
            supervisor,
            registry,
            mcp_pool,
            cancel:        Mutex::new(CancellationToken::new()),
            is_streaming:  AtomicBool::new(false),
            stream_state:  Mutex::new({
                let mut ss = StreamState::new();
                ss.tasks = octo_core::load_tasks(&data_dir(), "lair");
                ss
            }),
            ready_rx,
            relay_signer,
            relay_url:     relay_url_str,
            mgmt_token,
        });

        tokio::spawn(poll_agents(state.clone(), ready_tx.clone()));

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
            .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
            .allow_headers(Any);

        // Routes that mutate agent lifecycle — gated by `X-Octo-Token`. Peer
        // processes inside the container (i.e. child agents) don't get the
        // token in their env, so this is the wall that keeps children from
        // spawning siblings or terminating each other / lair via HTTP.
        let protected = Router::new()
            .route("/agents",             post(cli_create_agent))
            .route("/agents/:name/start", post(cli_start_agent))
            .route("/agents/:name/stop",  post(cli_stop_agent))
            .route("/agents/:name",       delete(cli_delete_agent))
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_mgmt_token,
            ));

        let app = Router::new()
            .route("/health",                  get(health_handler))
            .route("/info",                    get(info_handler))
            .route("/history",                 get(history_handler))
            .route("/stream",                  get(stream_handler))
            .route("/interrupt",               post(interrupt_handler))
            .route("/clear",                   post(clear_handler))
            .route("/agents",                  get(cli_list_agents))
            .route("/agents/:name/logs",       get(cli_agent_logs))
            .route("/agents/:name/stream",     get(proxy_agent_stream_handler))
            // Mobile-facing HTTP proxies for the child's existing endpoints.
            .route("/agents/:name/history",    get(proxy_agent_history))
            .route("/agents/:name/interrupt",  post(proxy_agent_interrupt))
            .route("/agents/:name/clear",      post(proxy_agent_clear))
            .route("/agents/:name/branches",   get(proxy_agent_branches))
            .merge(protected)
            .with_state(state)
            .layer(cors);

        let addr = format!("0.0.0.0:{http_port}");
        let listener = tokio::net::TcpListener::bind(&addr).await
            .map_err(|e| anyhow::anyhow!("failed to bind HTTP port {addr}: {e}"))?;
        info!("[lair] HTTP listening on {addr} (Noise proxy on 0.0.0.0:{noise_port})");

        crate::bootstrap::print_qr("lair", &public_host, public_port, &pubkey_b32);

        axum::serve(listener, app).await
            .map_err(|e| anyhow::anyhow!("axum serve error: {e}"))?;
    }
    Ok(())
}
