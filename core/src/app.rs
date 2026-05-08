//! Shared HTTP/WS plumbing used by both the lair (parent) and server (child)
//! binaries. Both embed an Axum HTTP server with a Noise-encrypted WebSocket;
//! this module captures the parts that are identical between them — buffer +
//! subscriber fanout for /stream, session persistence, wire-format conversion,
//! and the small ping/pong frame parsers.

use crate::{ApiMessage, ChatEvent, ContentBlock};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

// ── Buffer + subscriber fanout ────────────────────────────────────────────────

/// Live streaming state shared between the active streaming loop and any /stream
/// subscribers. Events are buffered for the current turn so a watcher joining
/// mid-turn replays everything they missed; the buffer is cleared at the start
/// of each new turn.
pub struct StreamState {
    pub buffer: Vec<String>,
    pub subs:   Vec<mpsc::UnboundedSender<String>>,
}

impl StreamState {
    pub fn new() -> Self { Self { buffer: Vec::new(), subs: Vec::new() } }
}

impl Default for StreamState {
    fn default() -> Self { Self::new() }
}

/// Push a JSON-serialized event to the per-turn buffer and fan it out to every
/// live WS subscriber. Subscribers whose receiver has been dropped are pruned.
pub fn buffer_and_fanout(state: &Mutex<StreamState>, json: String) {
    let mut ss = state.lock().unwrap();
    ss.buffer.push(json.clone());
    ss.subs.retain(|tx| tx.send(json.clone()).is_ok());
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
