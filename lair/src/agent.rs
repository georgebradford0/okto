//! Child agent role. Runs an HTTP+WS server bound to `127.0.0.1:<AGENT_PORT>`.
//! Mobile never connects directly — lair proxies WebSocket traffic from its
//! own Noise tunnel into this server on demand.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
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
        Path as AxumPath, Query, State,
    },
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use okto_core::{
    self,
    build_agent_system_prompt, build_system_prompt,
    build_tools_with_mcp, cancel_task as core_cancel_task, chain_executor_with_mcp,
    completion_chat_event, data_dir, finalize_task, from_base32, now_secs,
    monitor_complete_message, monitor_complete_text,
    monitor_process_tool, monitor_progress_message, monitor_progress_text,
    stop_monitor_tool,
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
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};

// ── Session persistence ───────────────────────────────────────────────────────

fn save_messages(dir: &Path, messages: &[ApiMessage]) {
    okto_core::save_messages(dir, messages, "agent");
}

fn load_messages(dir: &Path) -> Vec<ApiMessage> {
    okto_core::load_messages(dir, "agent")
}

// ── Worktrees ───────────────────────────────────────────────────────────────
//
// A worktree is a git worktree of this agent's own `workspace/.git`, living at
// `<agent_dir>/worktrees/<id>/`, with its own chat session. All worktrees share
// the agent's single clone and run under the agent's own uid, so there's no
// cross-process permission concern. The manifest at `<data_dir>/worktrees.json`
// is the source of truth for which worktrees exist; sessions are rebuilt from
// it on startup.

/// One worktree's persistent metadata. Serialized into `worktrees.json` and
/// surfaced to clients (which nest worktree chats under the agent row).
#[derive(Serialize, Deserialize, Clone, Debug)]
struct WorktreeMeta {
    /// Filesystem-safe id (derived from the branch); used in routes + as the
    /// session key.
    id:         String,
    /// Branch checked out in this worktree.
    branch:     String,
    /// Absolute path of the worktree working dir (`<agent_dir>/worktrees/<id>`).
    path:       String,
    /// Unix seconds at creation.
    created_at: u64,
}

fn manifest_path() -> PathBuf { data_dir().join("worktrees.json") }

fn load_worktree_manifest() -> Vec<WorktreeMeta> {
    match fs::read_to_string(manifest_path()) {
        Ok(t) if !t.trim().is_empty() => serde_json::from_str(&t).unwrap_or_else(|e| {
            warn!("[agent/worktrees] manifest corrupt ({e}); starting empty");
            Vec::new()
        }),
        _ => Vec::new(),
    }
}

fn save_worktree_manifest(metas: &[WorktreeMeta]) {
    let path = manifest_path();
    match serde_json::to_string_pretty(metas) {
        Ok(json) => {
            if let Err(e) = fs::write(&path, json) {
                warn!("[agent/worktrees] failed to write {}: {e}", path.display());
            }
        }
        Err(e) => warn!("[agent/worktrees] serialize manifest failed: {e}"),
    }
}

/// Filesystem-safe, route-safe id derived from a branch name (`feature/x` →
/// `feature-x`). Non-alphanumeric chars collapse to `-`; falls back to `wt`.
fn worktree_id_from_branch(branch: &str) -> String {
    let s: String = branch.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let trimmed = s.trim_matches('-').to_string();
    if trimmed.is_empty() { "wt".to_string() } else { trimmed }
}

/// Build a chat session for a worktree from its metadata, loading any persisted
/// history/tasks from `<data_dir>/worktrees/<id>/`. A worktree is always a repo,
/// so it gets the repo system prompt.
fn worktree_session(mcp_pool: &McpPool, meta: &WorktreeMeta) -> Arc<AppState> {
    let sdata = data_dir().join("worktrees").join(&meta.id);
    let messages = load_messages(&sdata);
    let mut ss = StreamState::new();
    ss.tasks = okto_core::load_tasks(&sdata, "agent");
    Arc::new(AppState {
        id:            meta.id.clone(),
        branch:        Some(meta.branch.clone()),
        data_dir:      sdata,
        messages:      Arc::new(Mutex::new(messages)),
        last_cost_usd: Mutex::new(None),
        system:        build_system_prompt(&meta.path),
        cwd:           meta.path.clone(),
        stream_state:  Mutex::new(ss),
        turn_gate:     Mutex::new(TurnGate::new()),
        cancel:        Mutex::new(CancellationToken::new()),
        mcp_pool:      mcp_pool.clone(),
    })
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

/// One chat session: a conversation bound to a single working directory. The
/// agent's main workspace is the default session (`id == ""`); each git
/// worktree gets its own session keyed by worktree id. All per-conversation
/// state (history, cwd, stream fanout, turn gate, cancel token, persistence
/// dir) lives here so one agent process can host several concurrently. The
/// `mcp_pool` is a cheap clone of the process-wide pool — connections are
/// shared, not duplicated per session.
struct AppState {
    /// Worktree id this session is bound to; `""` for the main workspace.
    id:            String,
    /// Branch checked out in this session's worktree. `None` for the main
    /// workspace (whatever branch the clone is on). Read at teardown to delete
    /// the branch along with the worktree.
    branch:        Option<String>,
    /// Directory this session persists its `session/messages.json` +
    /// `session/tasks.json` under. Main workspace = the agent's `data_dir()`;
    /// worktrees nest under `data_dir()/worktrees/<id>`.
    data_dir:      PathBuf,
    messages:      Arc<Mutex<Vec<ApiMessage>>>,
    last_cost_usd: Mutex<Option<f64>>,
    system:        String,
    cwd:           String,
    stream_state:  Mutex<StreamState>,
    turn_gate:     Mutex<TurnGate>,
    cancel:        Mutex<CancellationToken>,
    mcp_pool:      McpPool,
}

/// Process-level holder: every chat session this agent serves. The default
/// session (`""`) is the main workspace; git worktrees add more. HTTP handlers
/// resolve a session from here — the default one, or a `:worktree` path param —
/// and operate on it. Worktree lifecycle (add/list/remove) mutates `sessions`.
struct Agent {
    sessions:  Mutex<HashMap<String, Arc<AppState>>>,
    /// Worktree manifest — source of truth for which worktrees exist. Mirrors
    /// `worktrees.json`; mutated under this lock then persisted.
    worktrees: Mutex<Vec<WorktreeMeta>>,
    /// Process-wide MCP pool, cloned into each worktree session.
    mcp_pool:  McpPool,
    /// The agent's own dir (`/data/agents/<name>`): parent of `workspace/`,
    /// `worktrees/`, and `data/`. Used to lay out worktree dirs.
    agent_dir: PathBuf,
    /// The main git clone (`<agent_dir>/workspace`) whose `.git` every worktree
    /// attaches to.
    workspace: PathBuf,
}

impl Agent {
    /// The main-workspace session. Always present for the life of the process.
    fn default_session(&self) -> Arc<AppState> {
        self.sessions.lock().unwrap().get("")
            .cloned()
            .expect("default session is inserted at startup and never removed")
    }

    /// Look up a session by worktree id (`""` = main). `None` if no such
    /// worktree session exists.
    fn session(&self, id: &str) -> Option<Arc<AppState>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse { (StatusCode::OK, "ok") }

fn interrupt_session(s: &Arc<AppState>) -> StatusCode {
    s.cancel.lock().unwrap().cancel();
    s.turn_gate.lock().unwrap().interrupt_requested = true;
    StatusCode::OK
}

async fn interrupt_handler(State(agent): State<Arc<Agent>>) -> StatusCode {
    interrupt_session(&agent.default_session())
}

fn cancel_task_session(s: &Arc<AppState>, id: &str) -> Json<serde_json::Value> {
    let fired = core_cancel_task(&s.stream_state, id);
    info!("[agent/cancel_task] id={id} fired={fired}");
    Json(serde_json::json!({"id": id, "fired": fired}))
}

/// HTTP twin of the agent's `cancel_task` WS frame. POST /tasks/:id/cancel.
/// Same semantics as the WS path. Lair proxies `POST /agents/:name/tasks/:id/cancel`
/// here so operators can stop a child agent's background task from `okto tasks stop`.
async fn cancel_task_handler(
    AxumPath(id): AxumPath<String>,
    State(agent): State<Arc<Agent>>,
) -> Json<serde_json::Value> {
    cancel_task_session(&agent.default_session(), &id)
}

fn history_session(s: &Arc<AppState>) -> Json<serde_json::Value> {
    let cost = *s.last_cost_usd.lock().unwrap();
    let msgs = messages_to_history(&s.messages.lock().unwrap(), cost);
    Json(serde_json::json!({ "messages": msgs }))
}

async fn history_handler(State(agent): State<Arc<Agent>>) -> Json<serde_json::Value> {
    history_session(&agent.default_session())
}

async fn stream_handler(
    ws:           WebSocketUpgrade,
    State(agent): State<Arc<Agent>>,
) -> Response {
    let s = agent.default_session();
    ws.on_upgrade(move |socket| handle_stream(socket, s))
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
                    save_messages(&state.data_dir, &msgs);
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
            save_messages(&state.data_dir, &msgs);
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
                        save_messages(&state_arc.data_dir, &updated);
                        *state_arc.last_cost_usd.lock().unwrap() = Some(cost_usd);
                        done_tx.send(ChatEvent::Interrupted { cost_usd }).await.ok();
                    } else {
                        info!("[agent/stream] turn finished, cost=${cost_usd:.4}");
                        *msgs_arc.lock().unwrap() = updated.clone();
                        save_messages(&state_arc.data_dir, &updated);
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
                    save_messages(&state_arc.data_dir, &partial);
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
        save_messages(&state.data_dir, &msgs);
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
        save_messages(&state.data_dir, &msgs);
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

    let ready = serde_json::json!({"type":"ready","session_id":"","resumed":resumed,"model":resolve_model()}).to_string();
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
        // Tail marker so the client knows the replay is done and can atomically
        // swap its shadow turn state into view (avoids a truncate-then-rebuild
        // flash mid-turn). Only emitted when we actually replayed.
        if ws_tx.send(WsMessage::Text(r#"{"type":"replay_end"}"#.to_string())).await.is_err() { return; }
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

fn clear_session(s: &Arc<AppState>) -> StatusCode {
    info!("[agent/clear] clearing conversation history (session='{}')", s.id);
    let mut msgs = s.messages.lock().unwrap();
    msgs.clear();
    save_messages(&s.data_dir, &msgs);
    StatusCode::OK
}

async fn clear_handler(State(agent): State<Arc<Agent>>) -> StatusCode {
    clear_session(&agent.default_session())
}

#[derive(Deserialize)]
struct CompletionQuery { dir_part: Option<String>, file_part: Option<String> }

fn completions_session(s: &Arc<AppState>, p: CompletionQuery) -> Json<Vec<String>> {
    let dir_part  = p.dir_part.unwrap_or_default();
    let file_part = p.file_part.unwrap_or_default();
    let mut seen    = std::collections::HashSet::new();
    let mut results = Vec::new();
    let search_dir  = PathBuf::from(&s.cwd).join(&dir_part);
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

async fn get_completions_handler(
    State(agent): State<Arc<Agent>>,
    Query(p):     Query<CompletionQuery>,
) -> Json<Vec<String>> {
    completions_session(&agent.default_session(), p)
}

fn branches_session(s: &Arc<AppState>) -> Response {
    // A worktree's `.git` is a file (gitdir pointer), the main clone's is a
    // dir — accept either so branch listing works in both.
    if !PathBuf::from(&s.cwd).join(".git").exists() {
        return Json(Vec::<okto_core::Branch>::new()).into_response();
    }
    match get_branches_for_repo(&s.cwd) {
        Ok(b)  => Json(b).into_response(),
        Err(e) => {
            warn!("[agent/branches] failed to list branches for {}: {e}", s.cwd);
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

async fn get_branches_handler(State(agent): State<Arc<Agent>>) -> impl IntoResponse {
    branches_session(&agent.default_session())
}

// ── Worktree-scoped chat routes ─────────────────────────────────────────────
//
// `/worktrees/:wt/...` mirrors the top-level chat routes but targets a worktree
// session. Each resolves the session by id and 404s if it doesn't exist.

/// Resolve a worktree session or return a 404 response.
fn require_session(agent: &Arc<Agent>, wt: &str) -> Result<Arc<AppState>, Response> {
    agent.session(wt).ok_or_else(|| {
        (StatusCode::NOT_FOUND, format!("no worktree '{wt}'")).into_response()
    })
}

async fn history_handler_wt(
    AxumPath(wt): AxumPath<String>,
    State(agent): State<Arc<Agent>>,
) -> Response {
    match require_session(&agent, &wt) {
        Ok(s)  => history_session(&s).into_response(),
        Err(r) => r,
    }
}

async fn stream_handler_wt(
    ws:           WebSocketUpgrade,
    AxumPath(wt): AxumPath<String>,
    State(agent): State<Arc<Agent>>,
) -> Response {
    match require_session(&agent, &wt) {
        Ok(s)  => ws.on_upgrade(move |socket| handle_stream(socket, s)),
        Err(r) => r,
    }
}

async fn interrupt_handler_wt(
    AxumPath(wt): AxumPath<String>,
    State(agent): State<Arc<Agent>>,
) -> Response {
    match require_session(&agent, &wt) {
        Ok(s)  => interrupt_session(&s).into_response(),
        Err(r) => r,
    }
}

async fn clear_handler_wt(
    AxumPath(wt): AxumPath<String>,
    State(agent): State<Arc<Agent>>,
) -> Response {
    match require_session(&agent, &wt) {
        Ok(s)  => clear_session(&s).into_response(),
        Err(r) => r,
    }
}

async fn branches_handler_wt(
    AxumPath(wt): AxumPath<String>,
    State(agent): State<Arc<Agent>>,
) -> Response {
    match require_session(&agent, &wt) {
        Ok(s)  => branches_session(&s),
        Err(r) => r,
    }
}

async fn completions_handler_wt(
    AxumPath(wt): AxumPath<String>,
    State(agent): State<Arc<Agent>>,
    Query(p):     Query<CompletionQuery>,
) -> Response {
    match require_session(&agent, &wt) {
        Ok(s)  => completions_session(&s, p).into_response(),
        Err(r) => r,
    }
}

async fn cancel_task_handler_wt(
    AxumPath((wt, id)): AxumPath<(String, String)>,
    State(agent):       State<Arc<Agent>>,
) -> Response {
    match require_session(&agent, &wt) {
        Ok(s)  => cancel_task_session(&s, &id).into_response(),
        Err(r) => r,
    }
}

// ── Worktree lifecycle ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateWorktreeBody {
    branch: String,
    /// Optional base ref to branch from. Defaults to the repo's default branch.
    base:   Option<String>,
}

async fn list_worktrees_handler(State(agent): State<Arc<Agent>>) -> Json<Vec<WorktreeMeta>> {
    Json(agent.worktrees.lock().unwrap().clone())
}

/// POST /worktrees — create a new git worktree (on a new branch) plus its chat
/// session. The agent runs `git worktree add` against its own `workspace/.git`,
/// so the new tree shares the one clone and runs under this process's uid.
async fn create_worktree_handler(
    State(agent): State<Arc<Agent>>,
    Json(body):   Json<CreateWorktreeBody>,
) -> Response {
    let branch = body.branch.trim().to_string();
    if branch.is_empty() {
        return (StatusCode::BAD_REQUEST, "branch is required").into_response();
    }
    if !agent.workspace.join(".git").exists() {
        return (StatusCode::BAD_REQUEST, "this agent has no git repo to branch from").into_response();
    }
    let id = worktree_id_from_branch(&branch);
    if agent.session(&id).is_some() {
        return (StatusCode::CONFLICT, format!("worktree '{id}' already exists")).into_response();
    }

    let workspace = agent.workspace.to_string_lossy().to_string();
    let wt_dir    = agent.agent_dir.join("worktrees").join(&id);
    let wt_path   = wt_dir.to_string_lossy().to_string();
    if let Err(e) = fs::create_dir_all(agent.agent_dir.join("worktrees")) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("create worktrees dir: {e}")).into_response();
    }

    // Resolve the base ref (default branch) and add the worktree. Both are
    // blocking git invocations — run off the async runtime.
    let base = body.base.clone();
    let (ws, wtp, br) = (workspace.clone(), wt_path.clone(), branch.clone());
    let git_res = tokio::task::spawn_blocking(move || {
        let base = base.unwrap_or_else(|| {
            okto_core::git_default_base(&ws).unwrap_or_else(|_| "HEAD".to_string())
        });
        okto_core::add_worktree(&ws, &wtp, &br, &base)
    }).await;
    match git_res {
        Ok(Ok(()))  => {}
        Ok(Err(e))  => return (StatusCode::INTERNAL_SERVER_ERROR, format!("git worktree add: {e}")).into_response(),
        Err(e)      => return (StatusCode::INTERNAL_SERVER_ERROR, format!("worktree task panicked: {e}")).into_response(),
    }

    let meta = WorktreeMeta { id: id.clone(), branch: branch.clone(), path: wt_path, created_at: now_secs() };
    let session = worktree_session(&agent.mcp_pool, &meta);
    agent.sessions.lock().unwrap().insert(id.clone(), session);
    {
        let mut wts = agent.worktrees.lock().unwrap();
        wts.push(meta.clone());
        save_worktree_manifest(&wts);
    }
    info!("[agent/worktrees] created '{id}' on branch '{branch}'");
    (StatusCode::OK, Json(meta)).into_response()
}

/// DELETE /worktrees/:wt — tear down a worktree: cancel its turn, drop its
/// session, `git worktree remove` + delete its branch, and remove its chat
/// data dir.
async fn delete_worktree_handler(
    AxumPath(wt): AxumPath<String>,
    State(agent): State<Arc<Agent>>,
) -> Response {
    let session = agent.sessions.lock().unwrap().remove(&wt);
    let Some(session) = session else {
        return (StatusCode::NOT_FOUND, format!("no worktree '{wt}'")).into_response();
    };
    // Cancel any in-flight turn so the process can't write into a dir we're
    // about to remove.
    session.cancel.lock().unwrap().cancel();

    let workspace = agent.workspace.to_string_lossy().to_string();
    let wt_path   = session.cwd.clone();
    let branch    = session.branch.clone();
    let git_res = tokio::task::spawn_blocking(move || {
        okto_core::remove_worktree(&workspace, &wt_path, branch.as_deref())
    }).await;
    if let Ok(Err(e)) = git_res {
        warn!("[agent/worktrees] remove '{wt}': {e} (continuing teardown)");
    }
    // Drop the per-worktree chat data dir.
    let _ = fs::remove_dir_all(&session.data_dir);
    {
        let mut wts = agent.worktrees.lock().unwrap();
        wts.retain(|m| m.id != wt);
        save_worktree_manifest(&wts);
    }
    info!("[agent/worktrees] deleted '{wt}'");
    StatusCode::OK.into_response()
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
        stop_monitor_tool(),
        send_notification_tool(),
        okto_core::relay::ask_question_tool(),
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
                "stop_monitor"              => exec_stop_monitor(state, input).await,
                "spawn_agent"               => exec_spawn_agent(input).await,
                "terminate_agent"           => exec_terminate_agent(input).await,
                "send_notification"         => exec_send_notification(input).await,
                "ask_question"              => exec_ask_question(input).await,
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

/// `ask_question` tool — the child is blocked on the operator. Children hold
/// no relay signing key, so this forwards to lair (which signs + pushes) just
/// like `exec_send_notification`, but with the distinct `question` category.
/// Returns immediately telling the model to stop and wait; the operator's
/// answer arrives as their next chat message — there is no blocking round-trip.
async fn exec_ask_question(input: serde_json::Value) -> String {
    let question = input.get("question").and_then(|v| v.as_str()).unwrap_or("").trim();
    if question.is_empty() {
        return "error: 'question' is required".to_string();
    }
    forward_notify_to_lair(
        okto_core::relay::NOTIFY_CATEGORY_QUESTION, "okto has a question", question,
    ).await;
    "Your question was pushed to the operator's device. Stop here and wait for \
     their reply — it will arrive as their next message. Do not continue or take \
     other actions until they answer."
        .to_string()
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

/// Forward the raw outcome of a finished background task to lair, which runs
/// a one-shot LLM call to synthesise a user-friendly title/body before
/// signing and dispatching the push. Best-effort with a generous timeout to
/// cover the model call lair will make. Failures are swallowed.
async fn forward_task_notify_to_lair(category: &str, command: &str, status: &str, output: &str) {
    let Some(base) = std::env::var("LAIR_INTERNAL_URL").ok().filter(|s| !s.is_empty()) else {
        warn!("[agent/notify-task] LAIR_INTERNAL_URL unset — cannot forward push");
        return;
    };
    let agent = std::env::var("AGENT_NAME").ok().filter(|s| !s.is_empty());
    let url = format!("{base}/internal/notify-task");
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c)  => c,
        Err(e) => { warn!("[agent/notify-task] build http client failed: {e}"); return; }
    };
    let payload = serde_json::json!({
        "agent":    agent,
        "category": category,
        "command":  command,
        "status":   status,
        "output":   output,
    });
    match client.post(&url).json(&payload).send().await {
        Ok(r) if r.status().is_success() => debug!("[agent/notify-task] forwarded outcome to lair ({})", r.status()),
        Ok(r)  => warn!("[agent/notify-task] lair {url} returned {}", r.status()),
        Err(e) => warn!("[agent/notify-task] POST {url} failed: {e}"),
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
    let output = register_task(&state.stream_state, &state.data_dir, TaskRecord {
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
        finalize_task(&deliver_state.stream_state, &deliver_state.data_dir, &outcome);
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
        // forwards on our behalf — and runs a one-shot LLM call to turn the
        // raw outcome into a useful title/body. See `forward_task_notify_to_lair`.
        let command = outcome.command.clone();
        let status  = outcome.status.to_string();
        let output  = outcome.summary.clone();
        tokio::spawn(async move {
            forward_task_notify_to_lair("task_complete", &command, &status, &output).await;
        });

        try_continue_auto(deliver_state.clone());
    });
}

async fn exec_stop_monitor(state: Arc<AppState>, input: serde_json::Value) -> String {
    let task_id = match input.get("task_id").and_then(|v| v.as_str()).map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return "error: missing or empty 'task_id'".to_string(),
    };
    info!("[agent/stop_monitor] cancelling {task_id}");
    let fired = core_cancel_task(&state.stream_state, &task_id);
    if fired {
        format!("Stopped background process {task_id}. The process will be killed and the monitor loop will stop.")
    } else {
        format!("No running task found with id '{task_id}'. It may have already completed or the id is wrong.")
    }
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
    let output = register_task(&state.stream_state, &state.data_dir, TaskRecord {
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
        finalize_task(&deliver_state.stream_state, &deliver_state.data_dir, &outcome);
        buffer_and_fanout(&deliver_state.stream_state, tasks_wire_json(&deliver_state.stream_state));
        // Push notification. Children have no relay key, so lair signs and
        // forwards on our behalf — and runs a one-shot LLM call to turn the
        // raw outcome into a useful title/body. See `forward_task_notify_to_lair`.
        let command = outcome.command.clone();
        let status  = outcome.status.to_string();
        let output  = outcome.summary.clone();
        tokio::spawn(async move {
            forward_task_notify_to_lair("task_complete", &command, &status, &output).await;
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

    // Local children share lair's container, which already ran the bootstrap
    // script — re-running it per child would re-install everything. Only a
    // standalone (remote) agent, which is its own container's entrypoint,
    // runs it here. `OKTO_LOCAL_CHILD=1` is set by lair's supervisor when it
    // spawns a child in-container (see `agent_proc.rs`).
    if std::env::var("OKTO_LOCAL_CHILD").as_deref() != Ok("1") {
        crate::bootstrap::run_bootstrap_script("agent").await?;
    }

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
    let messages  = load_messages(&data_dir());
    info!(
        "[agent] loaded {} message(s) from history, cwd={cwd} (repo={})",
        messages.len(),
        if has_repo { "yes" } else { "no" },
    );

    let mcp_pool = init_mcp_pool().await;

    // The agent's own dir (`/data/agents/<name>`) is the parent of
    // `workspace/`; worktree dirs and per-worktree session data nest under it.
    let agent_dir = workspace.parent().map(PathBuf::from).unwrap_or_else(data_dir);

    // The main-workspace session (id ""). Persists under the agent's own
    // `data_dir()`; worktree sessions get their own subdirs.
    let default_session = Arc::new(AppState {
        id:            String::new(),
        branch:        None,
        data_dir:      data_dir(),
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
        mcp_pool:      mcp_pool.clone(),
    });

    // Rebuild worktree sessions from the manifest. A worktree whose dir went
    // missing out-of-band is dropped (and pruned from the manifest).
    let mut sessions: HashMap<String, Arc<AppState>> =
        HashMap::from([(String::new(), default_session)]);
    let mut worktrees: Vec<WorktreeMeta> = Vec::new();
    for meta in load_worktree_manifest() {
        if PathBuf::from(&meta.path).join(".git").exists() {
            info!("[agent] restoring worktree '{}' (branch '{}')", meta.id, meta.branch);
            sessions.insert(meta.id.clone(), worktree_session(&mcp_pool, &meta));
            worktrees.push(meta);
        } else {
            warn!("[agent] worktree '{}' path {} missing — pruning from manifest", meta.id, meta.path);
        }
    }
    if worktrees.len() != load_worktree_manifest().len() {
        save_worktree_manifest(&worktrees);
    }

    let agent = Arc::new(Agent {
        sessions:  Mutex::new(sessions),
        worktrees: Mutex::new(worktrees),
        mcp_pool,
        agent_dir,
        workspace: workspace.clone(),
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
        .route("/tasks/:id/cancel", post(cancel_task_handler))
        .route("/branches",    get(get_branches_handler))
        .route("/completions", get(get_completions_handler))
        .route("/config",      get(get_config_handler).put(update_config_handler))
        // Worktrees: lifecycle + per-worktree chat routes (mirror the top-level
        // chat routes, scoped to a worktree session).
        .route("/worktrees",                  get(list_worktrees_handler).post(create_worktree_handler))
        .route("/worktrees/:wt",              delete(delete_worktree_handler))
        .route("/worktrees/:wt/history",      get(history_handler_wt))
        .route("/worktrees/:wt/stream",       get(stream_handler_wt))
        .route("/worktrees/:wt/interrupt",    post(interrupt_handler_wt))
        .route("/worktrees/:wt/clear",        post(clear_handler_wt))
        .route("/worktrees/:wt/branches",     get(branches_handler_wt))
        .route("/worktrees/:wt/completions",  get(completions_handler_wt))
        .route("/worktrees/:wt/tasks/:id/cancel", post(cancel_task_handler_wt))
        .with_state(agent.clone())
        .layer(cors);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await
        .map_err(|e| {
            error!("[agent] failed to bind agent HTTP port {addr}: {e}");
            anyhow::anyhow!("failed to bind agent HTTP port {addr}: {e}")
        })?;
    info!("[agent] HTTP listening on {addr} (cwd: {})", agent.default_session().cwd);

    if let Ok(prompt) = std::env::var("STARTUP_PROMPT") {
        if !prompt.is_empty() {
            let state_sp   = agent.default_session();
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
                    save_messages(&state_sp.data_dir, &msgs);
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
                        save_messages(&state_sp.data_dir, &updated);
                        *state_sp.last_cost_usd.lock().unwrap() = Some(cost_usd);
                        info!("[agent] STARTUP_PROMPT complete cost=${cost_usd:.4}");
                    }
                    Err((e, mut partial)) => {
                        partial.push(ApiMessage {
                            role:    "error".to_string(),
                            content: vec![ContentBlock::Text { text: e.clone() }],
                        });
                        *state_sp.messages.lock().unwrap() = partial.clone();
                        save_messages(&state_sp.data_dir, &partial);
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
