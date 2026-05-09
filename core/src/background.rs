//! Shared `run_background_task` tool and dispatcher.
//!
//! Both lair (parent) and agent (child) expose this tool so the model can fan
//! off long-running work without blocking the current chat turn. The spawn
//! function is generic over a "deliver" closure: each role fans the completion
//! event out to its own /stream subscribers.

use crate::{
    send_message, AnthropicTool, ApiMessage, ChatEvent, ContentBlock,
};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

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

/// Spawn the background task as a detached tokio task. `deliver` is invoked on
/// completion (success or failure) so the caller can fan a system event into
/// its /stream and fire a push webhook.
pub fn spawn_background_task<F>(params: BackgroundTaskParams, deliver: F)
where
    F: FnOnce(BackgroundTaskResult) + Send + 'static,
{
    tokio::spawn(async move {
        let BackgroundTaskParams {
            task_id, task_description, system, model, api_key, cwd, extra_tools, extra_executor,
        } = params;

        info!("[background/{task_id}] starting task ({} chars)", task_description.len());

        let messages = vec![ApiMessage {
            role:    "user".to_string(),
            content: vec![ContentBlock::Text { text: task_description.clone() }],
        }];

        let cancel = CancellationToken::new();

        let result = send_message(
            messages,
            &system,
            &model,
            &api_key,
            &cwd,
            None,             // No live event stream — it's a background turn.
            cancel,
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
