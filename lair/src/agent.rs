//! Child agent role. Runs an HTTP+WS server bound to `127.0.0.1:<AGENT_PORT>`.
//! Mobile never connects directly — lair proxies WebSocket traffic from its
//! own Noise tunnel into this server on demand.

use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
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
    build_agent_system_prompt, build_system_prompt,
    build_tools_with_mcp, cancel_task as core_cancel_task, chain_executor_with_mcp,
    completion_chat_event, data_dir, finalize_task, now_secs, record_task_progress,
    register_task, tasks_wire_json, TaskRecord, TaskStatus,
    get_branches_for_repo, init_mcp_pool, init_shell_env,
    load_or_generate_keypair, read_config,
    resolve_api_key, resolve_model, run_command_in_background_tool,
    run_noise_proxy, send_message,
    spawn_background_command, to_base32, ApiMessage, AnthropicTool,
    BackgroundCommandParams, ChatEvent, Config, ContentBlock, McpPool,
    KEEPALIVE_INTERVAL, KEEPALIVE_MAX_MISSED,
    StreamState, buffer_and_fanout, chat_event_to_wire_json, messages_to_history,
    parse_ping_id, parse_pong_id, write_config,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use serde::Deserialize;
use tower_http::cors::{Any, CorsLayer};

// ── Session persistence ───────────────────────────────────────────────────────

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
    stream_state:  Mutex<StreamState>,
    is_streaming:  AtomicBool,
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
    Json(serde_json::json!({ "messages": msgs }))
}

async fn stream_handler(
    ws:           WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_stream(socket, state))
}

fn spawn_turn(state: Arc<AppState>, user_text: Option<String>) {
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

        if let Some(text) = user_text {
            let mut msgs = state.messages.lock().unwrap();
            msgs.push(ApiMessage {
                role:    "user".to_string(),
                content: vec![ContentBlock::Text { text }],
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

        while let Some(event) = event_rx.recv().await {
            if let Some(json) = chat_event_to_wire_json(&event) {
                buffer_and_fanout(&state.stream_state, json.to_string());
            }
        }
        state.is_streaming.store(false, Ordering::Relaxed);
        state.stream_state.lock().unwrap().buffer.clear();
        info!("[agent/stream] turn complete, is_streaming=false");

        try_continue_auto(state.clone());
    });
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
    info!("[agent/stream] auto-turn triggered by bg_complete");
    spawn_turn(state, None);
}

async fn handle_stream(socket: WebSocket, state: Arc<AppState>) {
    info!("[agent/stream] WebSocket connection opened");
    let (mut ws_tx, mut ws_rx) = socket.split();

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
    if ws_tx.send(WsMessage::Text(tasks_wire_json(&state.stream_state))).await.is_err() {
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
            spawn_turn(state.clone(), Some(text));
        }
        "interrupt" => {
            info!("[agent/stream] interrupt frame received");
            state.cancel.lock().unwrap().cancel();
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
    let mut tools = vec![run_command_in_background_tool()];
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
    std::env::var("OCTO_AGENT_TOKEN").ok().filter(|s| !s.is_empty()).is_some()
        && std::env::var("LAIR_INTERNAL_URL").ok().filter(|s| !s.is_empty()).is_some()
}

fn spawn_agent_tool() -> AnthropicTool {
    AnthropicTool {
        name: "spawn_agent".to_string(),
        description: "Spawn a new octo child agent owned by this agent. The new agent runs as \
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
                "spawn_agent"               => exec_spawn_agent(input).await,
                "terminate_agent"           => exec_terminate_agent(input).await,
                other => format!("unknown tool: {other}"),
            }
        })
    }))
}

async fn exec_spawn_agent(input: serde_json::Value) -> String {
    let Some(token) = std::env::var("OCTO_AGENT_TOKEN").ok().filter(|s| !s.is_empty()) else {
        return "error: this agent has no spawn capability (no OCTO_AGENT_TOKEN in env).".to_string();
    };
    let Some(base)  = std::env::var("LAIR_INTERNAL_URL").ok().filter(|s| !s.is_empty()) else {
        return "error: LAIR_INTERNAL_URL is unset; cannot reach lair management API.".to_string();
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c)  => c,
        Err(e) => return format!("error: build http client: {e}"),
    };
    let url = format!("{base}/agents/child");
    let resp = match client.post(&url)
        .header("X-Octo-Agent-Token", token)
        .json(&input)
        .send()
        .await
    {
        Ok(r)  => r,
        Err(e) => return format!("error: POST {url}: {e}"),
    };
    let status = resp.status();
    let body   = resp.text().await.unwrap_or_default();
    if status.is_success() { body } else { format!("error ({status}): {body}") }
}

async fn exec_terminate_agent(input: serde_json::Value) -> String {
    let name = match input.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return "error: missing 'name' field".to_string(),
    };
    let Some(token) = std::env::var("OCTO_AGENT_TOKEN").ok().filter(|s| !s.is_empty()) else {
        return "error: this agent has no terminate capability (no OCTO_AGENT_TOKEN in env).".to_string();
    };
    let Some(base)  = std::env::var("LAIR_INTERNAL_URL").ok().filter(|s| !s.is_empty()) else {
        return "error: LAIR_INTERNAL_URL is unset; cannot reach lair management API.".to_string();
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c)  => c,
        Err(e) => return format!("error: build http client: {e}"),
    };
    let url = format!("{base}/agents/child/{name}");
    let resp = match client.delete(&url)
        .header("X-Octo-Agent-Token", token)
        .send()
        .await
    {
        Ok(r)  => r,
        Err(e) => return format!("error: DELETE {url}: {e}"),
    };
    let status = resp.status();
    let body   = resp.text().await.unwrap_or_default();
    if status.is_success() {
        format!("Terminated '{name}' and any descendants.")
    } else {
        format!("error ({status}): {body}")
    }
}

async fn exec_run_command_in_background(state: Arc<AppState>, input: serde_json::Value) -> String {
    let command = match input.get("command").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return "error: missing or empty 'command'".to_string(),
    };

    let task_id = format!("bg-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    info!("[agent/run_command_in_background] spawning {task_id} ({} chars)", command.len());

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
        cwd:     state.cwd.clone(),
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

        try_continue_auto(stream_state_arc.clone());
    });

    format!("Background command {task_id} started. The user will be notified when it completes.")
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
        "ready_at": octo_core::now_secs(),
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
        tokio::spawn(run_noise_proxy(static_private, noise_port, port));
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
            ss.tasks = octo_core::load_tasks(&data_dir(), "agent");
            ss
        }),
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
        .map_err(|e| anyhow::anyhow!("failed to bind agent HTTP port {addr}: {e}"))?;
    info!("[agent] HTTP listening on {addr} (cwd: {})", state.cwd);

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
