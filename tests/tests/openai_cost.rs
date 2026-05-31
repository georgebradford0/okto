//! The OpenAI-compatible backend's per-turn cost accounting, driven fully
//! offline against the mock LLM's `/v1/chat/completions` endpoint.
//!
//! Anthropic has built-in per-token pricing; OpenAI-compatible providers don't,
//! so lair derives cost from the operator-set `cost_input1M` / `cost_output1M`
//! config fields (USD per 1M tokens) and falls back to 0.0 when either is
//! absent. The mock reports a fixed token usage so the expected dollar figure
//! is deterministic.

mod common;

use common::mock_llm::{MOCK_INPUT_TOKENS, MOCK_OUTPUT_TOKENS};
use common::{LairProcess, Turn};

/// Pull the `cost_usd` off the turn's `done` event.
fn turn_cost(events: &[serde_json::Value]) -> f64 {
    events
        .iter()
        .find(|e| e["type"] == "done")
        .expect("no done event")
        ["cost_usd"]
        .as_f64()
        .expect("cost_usd missing or not a number")
}

#[tokio::test]
async fn openai_cost_uses_configured_rates() {
    let cost_input_1m = 2.0; // USD / 1M input tokens
    let cost_output_1m = 6.0; // USD / 1M output tokens
    let lair = LairProcess::start_openai(
        vec![Turn::text("hi from openai mock")],
        Some(cost_input_1m),
        Some(cost_output_1m),
    )
    .await
    .expect("lair to start");

    let mut chat = lair.chat().await.expect("open chat ws");
    chat.send_user_message("hello").await.expect("send");
    let events = chat.collect_turn().await.expect("collect turn");

    // The mock streamed text, so the OpenAI path was actually exercised.
    let text: String = events
        .iter()
        .filter(|e| e["type"] == "text")
        .filter_map(|e| e["text"].as_str())
        .collect();
    assert_eq!(text, "hi from openai mock");

    let expected = (MOCK_INPUT_TOKENS as f64 * cost_input_1m
        + MOCK_OUTPUT_TOKENS as f64 * cost_output_1m)
        / 1_000_000.0;
    let got = turn_cost(&events);
    assert!(
        (got - expected).abs() < 1e-12,
        "cost {got} != expected {expected}"
    );
    assert!(got > 0.0, "configured rates should yield a positive cost");
}

#[tokio::test]
async fn openai_cost_is_zero_without_both_rates() {
    // Only the input rate is set → cost falls back to 0.0 (needs both).
    let lair = LairProcess::start_openai(
        vec![Turn::text("hi")],
        Some(2.0),
        None,
    )
    .await
    .expect("lair to start");

    let mut chat = lair.chat().await.expect("open chat ws");
    chat.send_user_message("hello").await.expect("send");
    let events = chat.collect_turn().await.expect("collect turn");

    assert_eq!(turn_cost(&events), 0.0, "cost should be 0.0 with only one rate set");
}
