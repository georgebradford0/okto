//! Shared HTTP/WS plumbing used by both the lair (parent) and server (child)
//! binaries. Both embed an Axum HTTP server with a Noise-encrypted WebSocket;
//! this module captures the parts that are identical between them — buffer +
//! subscriber fanout for /stream, session persistence, wire-format conversion,
//! and the small ping/pong frame parsers.

use crate::{now_secs, ApiMessage, ChatEvent, ContentBlock};
use crate::background::{TaskOutput, TaskRecord, TaskStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

// ── Buffer + subscriber fanout ────────────────────────────────────────────────

/// Live streaming state shared between the active streaming loop and any /stream
/// subscribers. Events are buffered for the current turn so a watcher joining
/// mid-turn replays everything they missed; the buffer is cleared at the start
/// of each new turn.
///
/// Also owns the per-chat background-task registry (`tasks`) and the live
/// cancel-token map (`task_cancellers`). Each lair / agent process has exactly
/// one StreamState — i.e. one chat — so a task spawned from this chat is
/// recorded here and only here. There is no cross-chat aggregation.
pub struct StreamState {
    pub buffer:           Vec<String>,
    pub subs:             Vec<mpsc::UnboundedSender<String>>,
    pub tasks:            Vec<TaskRecord>,
    /// Live cancel tokens keyed by task_id. Populated at spawn, removed when
    /// the deliver closure runs. Firing a token aborts the inner agentic loop
    /// and marks the record `Cancelled`. Tokens are runtime-only — they never
    /// outlive a process restart, which is why loaded `Running` records are
    /// rewritten to `Error` in `load_tasks`.
    pub task_cancellers:  HashMap<String, CancellationToken>,
    /// Live, bounded output buffers keyed by task_id. Populated at spawn by
    /// `register_task`, written by `spawn_background_command`, read by an
    /// attached monitor loop. Runtime-only; evicted with the registry cap.
    pub task_outputs:     HashMap<String, Arc<Mutex<TaskOutput>>>,
}

impl StreamState {
    pub fn new() -> Self {
        Self {
            buffer:          Vec::new(),
            subs:            Vec::new(),
            tasks:           Vec::new(),
            task_cancellers: HashMap::new(),
            task_outputs:    HashMap::new(),
        }
    }
}

impl Default for StreamState {
    fn default() -> Self { Self::new() }
}

/// Push a JSON-serialized event to the per-turn buffer and fan it out to every
/// live WS subscriber. Subscribers whose receiver has been dropped are pruned.
pub fn buffer_and_fanout(state: &Mutex<StreamState>, json: String) {
    let mut ss = state.lock().unwrap();
    ss.buffer.push(json.clone());
    let before = ss.subs.len();
    ss.subs.retain(|tx| tx.send(json.clone()).is_ok());
    let pruned = before - ss.subs.len();
    if pruned > 0 {
        debug!("[app] fanout pruned {pruned} dead subscriber(s), {} remain", ss.subs.len());
    }
}

// ── Session persistence ───────────────────────────────────────────────────────

/// Subdirectory of the data dir used to persist the chat session. Holds
/// `messages.json`, which is rewritten in full on every turn boundary.
pub fn session_dir(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("session")
}

pub fn save_messages(data_dir: &Path, messages: &[ApiMessage], log_prefix: &str) {
    let dir = session_dir(data_dir);
    std::fs::create_dir_all(&dir).ok();
    if let Ok(json) = serde_json::to_string(messages) {
        let path = dir.join("messages.json");
        if let Err(e) = std::fs::write(&path, json) {
            error!("[{log_prefix}] failed to save messages to {}: {e}", path.display());
        } else {
            debug!("[{log_prefix}] saved {} message(s) to {}", messages.len(), path.display());
        }
    }
}

pub fn load_messages(data_dir: &Path, log_prefix: &str) -> Vec<ApiMessage> {
    let path = session_dir(data_dir).join("messages.json");
    match std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()) {
        Some(msgs) => {
            let v: Vec<ApiMessage> = msgs;
            info!("[{log_prefix}] loaded {} message(s) from {}", v.len(), path.display());
            v
        }
        None => {
            debug!("[{log_prefix}] no saved messages at {}", path.display());
            vec![]
        }
    }
}

/// Persist the per-chat task registry to `<data_dir>/session/tasks.json`. The
/// full vector is rewritten on every change. Both lair and agent call this
/// whenever they mutate `StreamState.tasks` so a process restart can reload the
/// list (cancel tokens themselves are runtime-only — see `load_tasks`).
pub fn save_tasks(data_dir: &Path, tasks: &[TaskRecord], log_prefix: &str) {
    let dir = session_dir(data_dir);
    std::fs::create_dir_all(&dir).ok();
    if let Ok(json) = serde_json::to_string(tasks) {
        let path = dir.join("tasks.json");
        if let Err(e) = std::fs::write(&path, json) {
            error!("[{log_prefix}] failed to save tasks to {}: {e}", path.display());
        } else {
            debug!("[{log_prefix}] saved {} task(s) to {}", tasks.len(), path.display());
        }
    }
}

/// Load the per-chat task registry from disk. Any record still marked
/// `Running` is rewritten to `Error` with an explanatory summary because its
/// cancel token (and the inner tokio task) didn't survive the process restart.
pub fn load_tasks(data_dir: &Path, log_prefix: &str) -> Vec<TaskRecord> {
    let path = session_dir(data_dir).join("tasks.json");
    let raw: Vec<TaskRecord> = match std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()) {
        Some(t) => t,
        None    => {
            debug!("[{log_prefix}] no saved tasks at {}", path.display());
            return vec![];
        }
    };
    let now = now_secs();
    let mut tasks = raw;
    let mut orphaned = 0usize;
    for t in tasks.iter_mut() {
        if t.status == TaskStatus::Running {
            t.status       = TaskStatus::Error;
            t.completed_at = Some(now);
            t.summary      = Some("interrupted by server restart".to_string());
            orphaned += 1;
        }
    }
    if orphaned > 0 {
        warn!("[{log_prefix}] {orphaned} task(s) were Running at restart, marked Error");
    }
    info!("[{log_prefix}] loaded {} task(s) from {}", tasks.len(), path.display());
    tasks
}

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct HistMsg {
    pub role: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// Project the persisted `ApiMessage` log into the wire-shape `mobile/src/wire.ts`
/// expects for `GET /history`. Tool-result blocks attached to user messages are
/// folded into the preceding `tool` entry's `output` field; the cost (if any)
/// is attached to the most recent assistant message.
pub fn messages_to_history(messages: &[ApiMessage], last_cost_usd: Option<f64>) -> Vec<HistMsg> {
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
            "bg_complete" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                result.push(HistMsg { role: "bg_complete".to_string(), text, cost_usd: None, output: None });
            }
            "bg_progress" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                result.push(HistMsg { role: "bg_progress".to_string(), text, cost_usd: None, output: None });
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

/// Render a `ChatEvent` to the JSON shape the wire schema (`mobile/src/wire.ts`)
/// expects. Returns `None` for variants that aren't part of the /stream protocol.
pub fn chat_event_to_wire_json(event: &ChatEvent) -> Option<serde_json::Value> {
    match event {
        ChatEvent::Text { text } =>
            Some(serde_json::json!({"type":"text","text":text})),
        ChatEvent::ToolUse { tool, input, display } => {
            let mut v = serde_json::json!({"type":"tool_use","tool":tool,"input":input});
            if let Some(d) = display {
                v["display"] = serde_json::Value::String(d.clone());
            }
            Some(v)
        }
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
        ChatEvent::BgComplete { task_id, text } =>
            Some(serde_json::json!({"type":"bg_complete","task_id":task_id,"text":text})),
        ChatEvent::BgProgress { task_id, text } =>
            Some(serde_json::json!({"type":"bg_progress","task_id":task_id,"text":text})),
        _ => None,
    }
}

// ── Frame parsers ─────────────────────────────────────────────────────────────

/// Cheap parse for app-level `pong { id }` frames (handled per-WS, not via the
/// regular client-frame dispatcher). Returns the echoed id if `raw` is a valid
/// pong, else `None`.
pub fn parse_pong_id(raw: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if v.get("type").and_then(|x| x.as_str())? != "pong" { return None; }
    v.get("id").and_then(|x| x.as_u64())
}

/// Parse client → server `ping { id }` frames so we can answer with a `pong`
/// (mobile-side keepalive — symmetric to the server's outbound pings).
pub fn parse_ping_id(raw: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if v.get("type").and_then(|x| x.as_str())? != "ping" { return None; }
    v.get("id").and_then(|x| x.as_u64())
}
