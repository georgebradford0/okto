//! Phase 2 — a full chat turn over the tunnel: drive lair's own agentic loop
//! with a scripted mock model, assert the streamed event sequence, persisted
//! history, /clear, and mid-turn interrupt.

mod common;

use std::time::Duration;

use common::tunnel::ChatWs;
use common::{event_types, LairProcess, Turn};
use serde_json::Value;

/// Read events until one of `types` is seen; returns that event. Fails if the
/// stream closes first.
async fn read_until(chat: &mut ChatWs, types: &[&str]) -> Value {
    loop {
        let ev = chat
            .next_event()
            .await
            .expect("ws error")
            .expect("stream closed before expected event");
        if let Some(ty) = ev.get("type").and_then(|x| x.as_str()) {
            if types.contains(&ty) {
                return ev;
            }
        }
    }
}

#[tokio::test]
async fn text_turn_streams_ready_text_done() {
    let lair = LairProcess::start(vec![Turn::text("Hello from the mock")])
        .await
        .expect("lair to start");
    let mut chat = lair.chat().await.expect("open chat ws");

    // A `ready` frame is pushed on connect.
    let ready = read_until(&mut chat, &["ready"]).await;
    assert_eq!(ready["type"], "ready");

    chat.send_user_message("hi there").await.expect("send");
    let events = chat.collect_turn().await.expect("collect turn");
    let types = event_types(&events);

    assert!(types.contains(&"text".to_string()), "no text event in {types:?}");
    assert!(types.contains(&"done".to_string()), "no done event in {types:?}");

    let text: String = events
        .iter()
        .filter(|e| e["type"] == "text")
        .filter_map(|e| e["text"].as_str())
        .collect();
    assert_eq!(text, "Hello from the mock");

    // The mock was actually called by lair's loop.
    assert!(lair.mock.request_count() >= 1, "model was not called");
}

#[tokio::test]
async fn turn_is_persisted_to_history() {
    let lair = LairProcess::start(vec![Turn::text("persisted-marker-xyz")])
        .await
        .expect("lair to start");
    let mut chat = lair.chat().await.expect("open chat ws");
    read_until(&mut chat, &["ready"]).await;
    chat.send_user_message("remember this").await.expect("send");
    chat.collect_turn().await.expect("collect turn");

    // History is written as the turn finalizes; allow a brief settle.
    let mut found = false;
    for _ in 0..30 {
        let (status, body) = lair.http_get("/history").await.expect("GET /history");
        assert_eq!(status, 200);
        if body.contains("persisted-marker-xyz") && body.contains("remember this") {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(found, "turn not found in /history");
}

#[tokio::test]
async fn clear_wipes_history() {
    let lair = LairProcess::start(vec![Turn::text("ephemeral-abc")])
        .await
        .expect("lair to start");
    let mut chat = lair.chat().await.expect("open chat ws");
    read_until(&mut chat, &["ready"]).await;
    chat.send_user_message("hello").await.expect("send");
    chat.collect_turn().await.expect("collect turn");

    let (_, before) = lair.http_get("/history").await.expect("history");
    assert!(before.contains("ephemeral-abc"), "precondition: turn in history");

    let (status, _) = lair.http_post("/clear").await.expect("POST /clear");
    assert_eq!(status, 200, "clear status; log:\n{}", lair.log());

    let (_, after) = lair.http_get("/history").await.expect("history after clear");
    assert!(
        !after.contains("ephemeral-abc"),
        "history not cleared: {after}"
    );
}

#[tokio::test]
async fn interrupt_stops_an_in_flight_turn() {
    // First turn runs a slow command; we interrupt while it's executing so the
    // scripted follow-up text never lands.
    let lair = LairProcess::start(vec![
        Turn::tool("call_1", "bash", serde_json::json!({"command": "sleep 3"})),
        Turn::text("should-not-appear"),
    ])
    .await
    .expect("lair to start");

    let mut chat = lair.chat().await.expect("open chat ws");
    read_until(&mut chat, &["ready"]).await;
    chat.send_user_message("run something slow").await.expect("send");

    // Wait until the bash tool is actually running, then interrupt.
    let tool = read_until(&mut chat, &["tool_use"]).await;
    assert_eq!(tool["tool"], "bash");
    chat.interrupt().await.expect("interrupt");

    let terminal = read_until(&mut chat, &["interrupted", "done", "error"]).await;
    assert_eq!(
        terminal["type"], "interrupted",
        "expected the turn to be interrupted, got {terminal}"
    );
}

#[tokio::test]
async fn session_is_reusable_after_an_interrupt() {
    // Regression guard for the interrupt refactor: an interrupted turn must
    // release the gate cleanly and the *next* turn must run to completion on a
    // fresh, uncancelled token. Before the fix, the per-turn cancel token and
    // `interrupt_requested` were tracked under separate locks and swapped at
    // different times, leaving room for an interrupt to bleed across the turn
    // boundary. The interrupted bash turn never consumes the scripted text
    // turn, so it stays queued for the second user message.
    let lair = LairProcess::start(vec![
        Turn::tool("call_1", "bash", serde_json::json!({"command": "sleep 3"})),
        Turn::text("after-interrupt-ok"),
    ])
    .await
    .expect("lair to start");

    let mut chat = lair.chat().await.expect("open chat ws");
    read_until(&mut chat, &["ready"]).await;

    // Turn 1: interrupt mid-tool.
    chat.send_user_message("run something slow").await.expect("send");
    let tool = read_until(&mut chat, &["tool_use"]).await;
    assert_eq!(tool["tool"], "bash");
    chat.interrupt().await.expect("interrupt");
    let terminal = read_until(&mut chat, &["interrupted", "done", "error"]).await;
    assert_eq!(terminal["type"], "interrupted", "turn 1 should interrupt, got {terminal}");

    // Turn 2: a normal turn on the same session must complete with `done`
    // (not be mislabeled interrupted, and not be stuck behind a stale gate).
    chat.send_user_message("now answer normally").await.expect("send");
    let events = chat.collect_turn().await.expect("collect turn 2");
    let types = event_types(&events);
    assert!(
        types.contains(&"done".to_string()),
        "turn 2 did not complete with done: {types:?}\nlog:\n{}",
        lair.log()
    );
    let text: String = events
        .iter()
        .filter(|e| e["type"] == "text")
        .filter_map(|e| e["text"].as_str())
        .collect();
    assert_eq!(text, "after-interrupt-ok", "turn 2 text mismatch");
}
