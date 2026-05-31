//! A minimal mock LLM server for e2e tests.
//!
//! It speaks just enough of the Anthropic `/v1/messages` and the
//! OpenAI-compatible `/v1/chat/completions` streaming SSE protocols that
//! `okto_core::stream_anthropic` / `stream_openai` parse (see `pop_sse_event`
//! + the event matches in `core/src/lib.rs`). Point lair at the Anthropic
//! endpoint with `ANTHROPIC_API_URL`, or the OpenAI one with `OPENAI_API_URL`,
//! and script the turns each request should return.
//!
//! Both endpoints serve the same scripted [`Turn`] queue. The reported token
//! usage is fixed (`input_tokens`/`prompt_tokens` = 1, `output_tokens`/
//! `completion_tokens` = 5) so cost assertions are deterministic.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use serde_json::{json, Value};

/// One scripted model turn. The mock pops one of these per inbound request.
#[derive(Clone)]
pub enum Turn {
    /// Plain assistant text, then `stop_reason: end_turn`.
    Text(String),
    /// A single `tool_use` block, then `stop_reason: tool_use`. The agentic
    /// loop will execute the tool and call back for the next scripted turn.
    Tool {
        id: String,
        name: String,
        input: Value,
    },
}

impl Turn {
    pub fn text(s: impl Into<String>) -> Turn {
        Turn::Text(s.into())
    }
    pub fn tool(id: impl Into<String>, name: impl Into<String>, input: Value) -> Turn {
        Turn::Tool { id: id.into(), name: name.into(), input }
    }
}

/// Fixed token usage every scripted turn reports, on both backends. Tests use
/// these to compute the expected cost deterministically.
pub const MOCK_INPUT_TOKENS: u64 = 1;
pub const MOCK_OUTPUT_TOKENS: u64 = 5;

struct MockState {
    turns: Mutex<VecDeque<Turn>>,
    /// Every request body lair sent, in order — lets tests assert on what the
    /// model was actually asked (history growth, tool_result round-trip, etc).
    requests: Mutex<Vec<Value>>,
}

/// A running mock server. Drop it (or let the owning fixture drop) to stop.
#[derive(Clone)]
pub struct MockLlm {
    addr: String,
    state: Arc<MockState>,
}

impl MockLlm {
    /// Bind on an ephemeral loopback port and start serving the scripted turns.
    pub async fn start(turns: Vec<Turn>) -> anyhow::Result<MockLlm> {
        let state = Arc::new(MockState {
            turns: Mutex::new(turns.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/v1/messages", post(handle))
            .route("/v1/chat/completions", post(handle_openai))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(MockLlm {
            addr: format!("http://{addr}"),
            state,
        })
    }

    /// Full URL to pass to lair as `ANTHROPIC_API_URL` (Anthropic `/v1/messages`).
    pub fn url(&self) -> String {
        format!("{}/v1/messages", self.addr)
    }

    /// Full URL to pass to lair as `OPENAI_API_URL`
    /// (OpenAI-compatible `/v1/chat/completions`).
    pub fn openai_url(&self) -> String {
        format!("{}/v1/chat/completions", self.addr)
    }

    /// Number of model calls lair has made so far.
    pub fn request_count(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }

    /// The captured request bodies lair sent, in order.
    pub fn requests(&self) -> Vec<Value> {
        self.state.requests.lock().unwrap().clone()
    }
}

async fn handle(
    State(state): State<Arc<MockState>>,
    Json(body): Json<Value>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    state.requests.lock().unwrap().push(body);
    // Default to a terminal text turn if the script is exhausted, so the
    // agentic loop always converges instead of hanging.
    let turn = state
        .turns
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| Turn::Text("ok".to_string()));

    Sse::new(stream::iter(sse_events(turn)))
}

/// Build the ordered SSE events for one scripted turn, mirroring the shape
/// `stream_anthropic` decodes.
fn sse_events(turn: Turn) -> Vec<Result<Event, Infallible>> {
    let mut out: Vec<Result<Event, Infallible>> = Vec::new();
    let ev = |ty: &str, data: Value| -> Result<Event, Infallible> {
        Ok(Event::default().event(ty).data(data.to_string()))
    };

    out.push(ev(
        "message_start",
        json!({
            "type": "message_start",
            "message": { "usage": {
                "input_tokens": MOCK_INPUT_TOKENS,
                "output_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }}
        }),
    ));

    match turn {
        Turn::Text(text) => {
            out.push(ev(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ));
            out.push(ev(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            ));
            out.push(ev(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ));
            out.push(ev(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":MOCK_OUTPUT_TOKENS}}),
            ));
        }
        Turn::Tool { id, name, input } => {
            out.push(ev(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name}}),
            ));
            // Send the input as one input_json_delta chunk.
            out.push(ev(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),
            ));
            out.push(ev(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ));
            out.push(ev(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":MOCK_OUTPUT_TOKENS}}),
            ));
        }
    }

    out.push(ev("message_stop", json!({"type":"message_stop"})));
    out
}

/// OpenAI-compatible `/v1/chat/completions` handler. Pops the same scripted
/// turn queue and emits chat-completion SSE chunks that `stream_openai` parses.
async fn handle_openai(
    State(state): State<Arc<MockState>>,
    Json(body): Json<Value>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    state.requests.lock().unwrap().push(body);
    let turn = state
        .turns
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| Turn::Text("ok".to_string()));

    Sse::new(stream::iter(openai_sse_events(turn)))
}

/// Build the ordered SSE chunks for one scripted turn in OpenAI chat-completions
/// shape, mirroring what `stream_openai` decodes. The final chunk carries the
/// `usage` block (lair sets `stream_options.include_usage`).
fn openai_sse_events(turn: Turn) -> Vec<Result<Event, Infallible>> {
    let mut out: Vec<Result<Event, Infallible>> = Vec::new();
    // OpenAI chunks are unnamed `data:` lines (no SSE event name).
    let ev = |data: Value| -> Result<Event, Infallible> {
        Ok(Event::default().data(data.to_string()))
    };

    match turn {
        Turn::Text(text) => {
            out.push(ev(json!({
                "choices": [{"index":0,"delta":{"content":text},"finish_reason":null}]
            })));
            out.push(ev(json!({
                "choices": [{"index":0,"delta":{},"finish_reason":"stop"}]
            })));
        }
        Turn::Tool { id, name, input } => {
            out.push(ev(json!({
                "choices": [{"index":0,"delta":{"tool_calls":[{
                    "index":0,"id":id,
                    "function":{"name":name,"arguments":input.to_string()}
                }]},"finish_reason":null}]
            })));
            out.push(ev(json!({
                "choices": [{"index":0,"delta":{},"finish_reason":"tool_calls"}]
            })));
        }
    }

    // Final usage-only chunk (choices empty), as OpenAI sends when
    // include_usage is set.
    out.push(ev(json!({
        "choices": [],
        "usage": {
            "prompt_tokens": MOCK_INPUT_TOKENS,
            "completion_tokens": MOCK_OUTPUT_TOKENS,
            "total_tokens": MOCK_INPUT_TOKENS + MOCK_OUTPUT_TOKENS
        }
    })));
    out.push(Ok(Event::default().data("[DONE]")));
    out
}
