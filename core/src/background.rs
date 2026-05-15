//! Shared `run_command_in_background` tool and dispatcher.
//!
//! Both lair (parent) and agent (child) expose this tool so the model can fan
//! off long-running shell commands without blocking the current chat turn. The
//! spawn function is generic over a "deliver" closure: each role fans the
//! completion event out to its own /stream subscribers.

use crate::{now_secs, AnthropicTool, ChatEvent};
use crate::app::StreamState;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// How often to mirror the running command's output tail into the registry.
/// Cheap (in-memory only) so 1 s is fine.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(1000);

/// Keep the trailing `max` chars of `s`. For a long-running polling loop the
/// most recent output is what the user actually wants to see.
fn tail_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        s.chars().skip(n - max).collect()
    }
}

/// Build the combined stdout/stderr snapshot the registry stores as a task's
/// `summary`. Matches the format used in the final outcome on completion so
/// the live and post-completion views look the same.
fn combined_snapshot(stdout: &str, stderr: &str) -> String {
    if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n[stderr]: {stderr}")
    }
}

/// Most recent N tasks retained per chat. Older entries are dropped when this
/// cap is exceeded so the registry can't grow unbounded across a long-lived
/// session.
pub const MAX_TASKS_RETAINED: usize = 50;

/// Per-task summary text stored in the registry is capped at this many chars.
/// Keeps the `tasks` wire frame small even when a background command produces
/// a very long output. Mirrors the truncation in `completion_chat_event`.
pub const MAX_TASK_SUMMARY: usize = 800;

/// Lifecycle state of a tracked background task.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus { Running, Done, Error, Cancelled }

/// Per-chat record of a background task — created when the command is spawned
/// and updated when it completes. Serialised straight into the `tasks` wire
/// frame so the field names here are part of the public schema. Also
/// persisted to disk (`<data_dir>/session/tasks.json`) so a process restart
/// preserves the per-chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id:      String,
    pub command:      String,
    pub status:       TaskStatus,
    /// Unix epoch seconds.
    pub started_at:   u64,
    pub completed_at: Option<u64>,
    pub summary:      Option<String>,
    pub cost_usd:     Option<f64>,
}

/// Build the AnthropicTool spec for `run_command_in_background`.
pub fn run_command_in_background_tool() -> AnthropicTool {
    AnthropicTool {
        name: "run_command_in_background".to_string(),
        description: "Run a shell command in the background and return immediately. \
                      The command is executed with `bash -c` and its stdout/stderr is \
                      captured. When it finishes, the user is notified in-app via a \
                      system message and the output is injected into this conversation. \
                      Use this for commands that would otherwise tie up the current turn \
                      for minutes — long builds, big test suites, large downloads. \
                      Do not use it for fast commands; prefer the regular `bash` tool."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to run, executed via `bash -c`."
                }
            },
            "required": ["command"]
        }),
        display_label: Some("Running command in background".into()),
    }
}

/// Parameters for spawning a background command.
pub struct BackgroundCommandParams {
    pub task_id: String,
    pub command: String,
    pub cwd:     String,
}

/// Outcome handed to the deliver closure when the background command finishes.
pub struct BackgroundCommandResult {
    pub task_id:  String,
    pub command:  String,
    pub status:   &'static str,
    pub summary:  String,
    pub cost_usd: f64,
}

/// Spawn the background command as a detached tokio task. The caller supplies
/// the `cancel` token so it can register the task in the per-chat registry
/// *before* spawning — closing the small race where the tokio task could
/// deliver before the record exists.
///
/// `progress` is invoked at most once every `PROGRESS_INTERVAL` while output
/// is flowing, with the current output tail (last `MAX_TASK_SUMMARY` chars of
/// combined stdout+stderr). It will not fire when nothing has changed since
/// the last tick, so a silent polling loop is cheap.
///
/// `deliver` is invoked on completion (success, failure, *or* cancellation)
/// so the caller can fan a system event into its /stream and fire a push
/// webhook.
pub fn spawn_background_command<P, F>(
    params:   BackgroundCommandParams,
    cancel:   CancellationToken,
    progress: P,
    deliver:  F,
)
where
    F: FnOnce(BackgroundCommandResult) + Send + 'static,
    P: Fn(&str) + Send + 'static,
{
    tokio::spawn(async move {
        let BackgroundCommandParams { task_id, command, cwd } = params;

        info!("[background/{task_id}] running ({} chars) cwd={cwd}", command.len());

        let spawn_result = tokio::process::Command::new("bash")
            .arg("-c").arg(&command)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        let outcome = match spawn_result {
            Err(e) => {
                error!("[background/{task_id}] spawn failed: {e}");
                BackgroundCommandResult {
                    task_id: task_id.clone(),
                    command,
                    status: "error",
                    summary: format!("spawn failed: {e}"),
                    cost_usd: 0.0,
                }
            }
            Ok(mut child) => {
                let stdout_pipe = child.stdout.take().expect("stdout piped");
                let stderr_pipe = child.stderr.take().expect("stderr piped");
                let mut stdout_reader = TokioBufReader::new(stdout_pipe).lines();
                let mut stderr_reader = TokioBufReader::new(stderr_pipe).lines();
                let mut stdout_buf = String::new();
                let mut stderr_buf = String::new();
                let mut cancelled = false;
                let mut dirty = false;
                let mut tick = tokio::time::interval(PROGRESS_INTERVAL);

                loop {
                    tokio::select! {
                        line = stdout_reader.next_line() => match line {
                            Ok(Some(l)) => { stdout_buf.push_str(&l); stdout_buf.push('\n'); dirty = true; }
                            _ => break,
                        },
                        line = stderr_reader.next_line() => match line {
                            Ok(Some(l)) => { stderr_buf.push_str(&l); stderr_buf.push('\n'); dirty = true; }
                            _ => break,
                        },
                        _ = tick.tick() => {
                            if dirty {
                                let combined = combined_snapshot(&stdout_buf, &stderr_buf);
                                progress(&tail_chars(&combined, MAX_TASK_SUMMARY));
                                dirty = false;
                            }
                        }
                        _ = cancel.cancelled() => {
                            child.kill().await.ok();
                            cancelled = true;
                            break;
                        }
                    }
                }
                while let Ok(Some(l)) = stdout_reader.next_line().await {
                    stdout_buf.push_str(&l); stdout_buf.push('\n');
                }
                while let Ok(Some(l)) = stderr_reader.next_line().await {
                    stderr_buf.push_str(&l); stderr_buf.push('\n');
                }
                let status = child.wait().await.ok();

                let combined = combined_snapshot(&stdout_buf, &stderr_buf);

                let (status_str, summary) = if cancelled {
                    info!("[background/{task_id}] cancelled");
                    ("cancelled", format!("cancelled by user\n\n{combined}"))
                } else {
                    let exit_ok = status.as_ref().map(|s| s.success()).unwrap_or(false);
                    let exit_code = status.and_then(|s| s.code());
                    if exit_ok {
                        info!("[background/{task_id}] done (exit 0, {} chars)", combined.len());
                        ("done", combined)
                    } else {
                        error!("[background/{task_id}] failed (exit {exit_code:?})");
                        let header = match exit_code {
                            Some(c) => format!("exit code {c}\n\n"),
                            None    => "process killed by signal\n\n".to_string(),
                        };
                        ("error", format!("{header}{combined}"))
                    }
                };

                BackgroundCommandResult {
                    task_id: task_id.clone(),
                    command,
                    status: status_str,
                    summary,
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
    let task_id = record.task_id.clone();
    let snapshot = {
        let mut ss = state.lock().unwrap();
        ss.task_cancellers.insert(record.task_id.clone(), cancel);
        ss.tasks.push(record);
        if ss.tasks.len() > MAX_TASKS_RETAINED {
            let drop = ss.tasks.len() - MAX_TASKS_RETAINED;
            debug!("[background] task registry over cap, evicting {drop} oldest");
            ss.tasks.drain(0..drop);
        }
        ss.tasks.clone()
    };
    debug!("[background] registered task {task_id} ({} in registry)", snapshot.len());
    crate::app::save_tasks(data_dir, &snapshot, "tasks");
}

/// Mark a task complete in the registry, persist the result, and drop its
/// cancel-token entry. No-op if the id has fallen out of the retention window.
pub fn finalize_task(
    state:    &Mutex<StreamState>,
    data_dir: &Path,
    outcome:  &BackgroundCommandResult,
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
            t.summary  = Some(tail_chars(&outcome.summary, MAX_TASK_SUMMARY));
            t.cost_usd = Some(outcome.cost_usd);
        } else {
            debug!("[background] finalize: task {} fell out of retention window", outcome.task_id);
        }
        ss.tasks.clone()
    };
    debug!("[background] finalized task {} status={}", outcome.task_id, outcome.status);
    crate::app::save_tasks(data_dir, &snapshot, "tasks");
}

/// Mid-flight progress update: store the latest output tail on the running
/// task's record. Caller follows up with a `tasks_wire_json` fan-out so live
/// /stream subscribers see the tick. In-memory only — the live tail is
/// ephemeral and doesn't warrant an fsync on every update.
pub fn record_task_progress(state: &Mutex<StreamState>, task_id: &str, output_tail: &str) {
    let mut ss = state.lock().unwrap();
    if let Some(t) = ss.tasks.iter_mut().find(|t| t.task_id == task_id) {
        t.summary = Some(output_tail.to_string());
    }
}

/// Trigger cancellation of a running task by id. Returns true if a live cancel
/// token was found and fired. The deliver closure registered at spawn time
/// will run shortly after, marking the record `Cancelled` and pushing the
/// updated tasks frame.
pub fn cancel_task(state: &Mutex<StreamState>, task_id: &str) -> bool {
    let token = state.lock().unwrap().task_cancellers.get(task_id).cloned();
    if let Some(token) = token {
        debug!("[background] cancelling task {task_id}");
        token.cancel();
        true
    } else {
        warn!("[background] cancel requested for unknown/finished task {task_id}");
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

/// Render the system event a background command emits when complete.
pub fn completion_chat_event(outcome: &BackgroundCommandResult) -> ChatEvent {
    let prefix = match outcome.status {
        "done" => "✓",
        _      => "✗",
    };
    let total = outcome.summary.chars().count();
    let preview = tail_chars(&outcome.summary, MAX_TASK_SUMMARY);
    let truncated = if total > MAX_TASK_SUMMARY { " (truncated)" } else { "" };
    ChatEvent::System {
        text: format!(
            "{prefix} background command {} {}: {preview}{truncated}",
            outcome.task_id, outcome.status,
        ),
    }
}
