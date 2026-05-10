//! Shared `run_background_task` tool and dispatcher.
//!
//! Both lair (parent) and agent (child) expose this tool so the model can fan
//! off long-running work without blocking the current chat turn. The spawn
//! function is generic over a "deliver" closure: each role fans the completion
//! event out to its own /stream subscribers.

use crate::{
    now_secs, send_message, AnthropicTool, ApiMessage, ChatEvent, ContentBlock,
};
use crate::app::StreamState;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Most recent N tasks retained per chat. Older entries are dropped when this
/// cap is exceeded so the registry can't grow unbounded across a long-lived
/// session.
pub const MAX_TASKS_RETAINED: usize = 50;

/// Per-task summary text stored in the registry is capped at this many chars.
/// Keeps the `tasks` wire frame small even when a background task produces a
/// very long final response. Mirrors the truncation in `completion_chat_event`.
pub const MAX_TASK_SUMMARY: usize = 800;

/// Lifecycle state of a tracked background task.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus { Running, Done, Error, Cancelled }

/// Per-chat record of a background task — created when the task is spawned
/// and updated when it completes. Serialised straight into the `tasks` wire
/// frame so the field names here are part of the public schema. Also
/// persisted to disk (`<data_dir>/session/tasks.json`) so a process restart
/// preserves the per-chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id:          String,
    pub task_description: String,
    pub status:           TaskStatus,
    /// Unix epoch seconds.
    pub started_at:       u64,
    pub completed_at:     Option<u64>,
    pub summary:          Option<String>,
    pub cost_usd:         Option<f64>,
}

/// Build the AnthropicTool spec for `run_background_task`.
pub fn run_background_task_tool() -> AnthropicTool {
    AnthropicTool {
        name: "run_background_task".to_string(),
        description: "Spawn a long-running task in the background and return immediately. \
                      The task runs as an isolated agentic loop with the same tools you have, \
                      starting from `task_description` as its first user message. \
                      When it finishes, the user is notified in-app via a system message. \
                      Use this for work that would otherwise tie up the current turn for minutes \
                      — long builds, big test suites, repo-wide refactors, multi-step research. \
                      Do not use it for trivially short tasks; the user prefers a direct reply."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "task_description": {
                    "type": "string",
                    "description": "A self-contained prompt the background agent will receive as its \
                                    first user message. Include all context it needs — it does not \
                                    inherit the current conversation history."
                }
            },
            "required": ["task_description"]
        }),
        display_label: Some("Spawning background task".into()),
    }
}

/// Parameters for spawning a background task. The caller provides everything
/// needed to run an agentic loop independently of the current turn.
pub struct BackgroundTaskParams {
    pub task_id:        String,
    pub task_description: String,
    pub system:         String,
    pub model:          String,
    pub api_key:        String,
    pub cwd:            String,
    pub extra_tools:    Vec<AnthropicTool>,
    pub extra_executor: Option<Arc<dyn Fn(String, serde_json::Value)
                                -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
                                + Send + Sync>>,
}

/// Outcome handed to the deliver closure when the background task finishes.
pub struct BackgroundTaskResult {
    pub task_id: String,
    pub task_description: String,
    pub status:  &'static str,
    pub summary: String,
    pub cost_usd: f64,
}

/// Spawn the background task as a detached tokio task. The caller supplies the
/// `cancel` token so it can register the task in the per-chat registry *before*
/// spawning — closing the small race where the tokio task could deliver before
/// the record exists.
///
/// `deliver` is invoked on completion (success, failure, *or* cancellation) so
/// the caller can fan a system event into its /stream and fire a push webhook.
pub fn spawn_background_task<F>(
    params:  BackgroundTaskParams,
    cancel:  CancellationToken,
    deliver: F,
)
where
    F: FnOnce(BackgroundTaskResult) + Send + 'static,
{
    let cancel_inner = cancel.clone();

    tokio::spawn(async move {
        let BackgroundTaskParams {
            task_id, task_description, system, model, api_key, cwd, extra_tools, extra_executor,
        } = params;

        info!("[background/{task_id}] starting task ({} chars)", task_description.len());

        let messages = vec![ApiMessage {
            role:    "user".to_string(),
            content: vec![ContentBlock::Text { text: task_description.clone() }],
        }];

        let result = send_message(
            messages,
            &system,
            &model,
            &api_key,
            &cwd,
            None,             // No live event stream — it's a background turn.
            cancel_inner.clone(),
            &extra_tools,
            extra_executor,
        ).await;

        let outcome = match result {
            Ok((text, cost_usd, _)) => {
                info!(
                    "[background/{task_id}] done cost=${cost_usd:.4} response=({} chars)",
                    text.len()
                );
                BackgroundTaskResult {
                    task_id: task_id.clone(),
                    task_description,
                    status: "done",
                    summary: text,
                    cost_usd,
                }
            }
            Err((e, _)) if cancel_inner.is_cancelled() => {
                info!("[background/{task_id}] cancelled");
                BackgroundTaskResult {
                    task_id: task_id.clone(),
                    task_description,
                    status: "cancelled",
                    summary: format!("cancelled by user: {e}"),
                    cost_usd: 0.0,
                }
            }
            Err((e, _)) => {
                error!("[background/{task_id}] error: {e}");
                BackgroundTaskResult {
                    task_id: task_id.clone(),
                    task_description,
                    status: "error",
                    summary: e,
                    cost_usd: 0.0,
                }
            }
        };

        deliver(outcome);
    });
}

/// Append a freshly-spawned task's record to the per-chat registry, evicting
/// the oldest entries when the cap is exceeded. Persists the snapshot to disk
/// at `<data_dir>/session/tasks.json` so a process restart preserves the list.
pub fn register_task(
    state:    &Mutex<StreamState>,
    data_dir: &Path,
    record:   TaskRecord,
    cancel:   CancellationToken,
) {
    let snapshot = {
        let mut ss = state.lock().unwrap();
        ss.task_cancellers.insert(record.task_id.clone(), cancel);
        ss.tasks.push(record);
        if ss.tasks.len() > MAX_TASKS_RETAINED {
            let drop = ss.tasks.len() - MAX_TASKS_RETAINED;
            ss.tasks.drain(0..drop);
        }
        ss.tasks.clone()
    };
    crate::app::save_tasks(data_dir, &snapshot, "tasks");
}

/// Mark a task complete in the registry, persist the result, and drop its
/// cancel-token entry. No-op if the id has fallen out of the retention window.
pub fn finalize_task(
    state:    &Mutex<StreamState>,
    data_dir: &Path,
    outcome:  &BackgroundTaskResult,
) {
    let snapshot = {
        let mut ss = state.lock().unwrap();
        ss.task_cancellers.remove(&outcome.task_id);
        if let Some(t) = ss.tasks.iter_mut().find(|t| t.task_id == outcome.task_id) {
            t.status = match outcome.status {
                "done"      => TaskStatus::Done,
                "cancelled" => TaskStatus::Cancelled,
                _           => TaskStatus::Error,
            };
            t.completed_at = Some(now_secs());
            let summary: String = outcome.summary.chars().take(MAX_TASK_SUMMARY).collect();
            t.summary  = Some(summary);
            t.cost_usd = Some(outcome.cost_usd);
        }
        ss.tasks.clone()
    };
    crate::app::save_tasks(data_dir, &snapshot, "tasks");
}

/// Trigger cancellation of a running task by id. Returns true if a live cancel
/// token was found and fired. The deliver closure registered at spawn time
/// will run shortly after, marking the record `Cancelled` and pushing the
/// updated tasks frame.
pub fn cancel_task(state: &Mutex<StreamState>, task_id: &str) -> bool {
    let token = state.lock().unwrap().task_cancellers.get(task_id).cloned();
    if let Some(token) = token {
        token.cancel();
        true
    } else {
        false
    }
}

/// Build the JSON wire frame for a tasks snapshot. Caller pushes this through
/// `buffer_and_fanout` (live update) or sends it directly to a freshly-opened
/// /stream WS so the client has the registry without an extra HTTP round-trip.
pub fn tasks_wire_json(state: &Mutex<StreamState>) -> String {
    let payload = {
        let ss = state.lock().unwrap();
        serde_json::to_value(&ss.tasks).unwrap_or(serde_json::Value::Array(vec![]))
    };
    serde_json::json!({"type":"tasks","tasks":payload}).to_string()
}

/// Render the system event a background task emits when complete.
pub fn completion_chat_event(outcome: &BackgroundTaskResult) -> ChatEvent {
    let prefix = match outcome.status {
        "done" => "✓",
        _      => "✗",
    };
    let preview: String = outcome.summary.chars().take(800).collect();
    let truncated = if outcome.summary.len() > preview.len() { " (truncated)" } else { "" };
    ChatEvent::System {
        text: format!(
            "{prefix} background task {} {}: {preview}{truncated}",
            outcome.task_id, outcome.status,
        ),
    }
}
