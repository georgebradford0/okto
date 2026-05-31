//! `okto init --disable-push` end-to-end.
//!
//! The CLI flag persists `OKTO_RELAY_URL=` (empty) into the lair container env.
//! These tests boot the real `lair` binary with that env var set both ways and
//! assert the two observable surfaces the flag is supposed to change:
//!
//!   1. `/info` advertises an empty `relay_url` (so the mobile client's
//!      `registerWithRelay.ts` no-ops instead of registering for APNs).
//!   2. The model never sees the `send_notification` / `ask_question` tools in
//!      its tool list — they are dropped from `lair_extra_tools` rather than
//!      registered and stubbed.
//!
//! A paired "push enabled" test boots with a custom `OKTO_RELAY_URL` and
//! asserts the inverse, so the gating contract is self-evident.

mod common;

use common::{LairProcess, Turn};
use serde_json::Value;

/// Collect every tool name lair offered the model across all captured
/// requests. The mock LLM records the raw JSON body of each `/v1/messages`
/// call, which is exactly what `okto_core` POSTs to Anthropic — tool names
/// live in `request.tools[].name`.
fn captured_tool_names(lair: &LairProcess) -> Vec<String> {
    let mut names = Vec::new();
    for req in lair.mock.requests() {
        let Some(tools) = req.get("tools").and_then(Value::as_array) else { continue };
        for t in tools {
            if let Some(n) = t.get("name").and_then(Value::as_str) {
                names.push(n.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[tokio::test]
async fn disable_push_empties_info_relay_url_and_hides_push_tools() {
    // `OKTO_RELAY_URL=""` is the on-wire signal `okto init --disable-push`
    // writes into lair-env.
    let lair = LairProcess::start_with_env(
        vec![Turn::text("hello")],
        &[("OKTO_RELAY_URL", "")],
    )
    .await
    .expect("lair to start");

    // --- /info: mobile uses this to decide whether to register for APNs.
    let (status, body) = lair.http_get("/info").await.expect("GET /info");
    assert_eq!(status, 200, "info status; log:\n{}", lair.log());
    let info: Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("info body not JSON ({e}): {body:?}"));
    assert_eq!(
        info.get("relay_url").and_then(Value::as_str),
        Some(""),
        "expected empty relay_url in /info, got {info}",
    );

    // --- Tool list: drive one chat turn so lair actually calls the mock with
    //     its `tools` array, then introspect what tool names it sent.
    let mut chat = lair.chat().await.expect("open chat ws");
    chat.send_user_message("hi").await.expect("send");
    chat.collect_turn().await.expect("collect turn");

    let tools = captured_tool_names(&lair);
    assert!(
        !tools.iter().any(|n| n == "send_notification"),
        "send_notification should be hidden when push is disabled; tools={tools:?}",
    );
    assert!(
        !tools.iter().any(|n| n == "ask_question"),
        "ask_question should be hidden when push is disabled; tools={tools:?}",
    );
    // Sanity: other lair tools are still registered, so the absence above is
    // because of the gating — not because the tools array was empty.
    assert!(
        tools.iter().any(|n| n == "bash"),
        "expected `bash` to still be present as a control; tools={tools:?}",
    );
}

#[tokio::test]
async fn push_tools_are_present_when_relay_url_is_set() {
    // Default `start()` already sets OKTO_RELAY_URL to http://127.0.0.1:1 — a
    // non-empty value, so push is enabled and the tools should appear in the
    // model's tool list. We don't actually need the relay to be reachable;
    // this only verifies the gating contract.
    let lair = LairProcess::start(vec![Turn::text("hello")])
        .await
        .expect("lair to start");

    let (status, body) = lair.http_get("/info").await.expect("GET /info");
    assert_eq!(status, 200, "info status; log:\n{}", lair.log());
    let info: Value = serde_json::from_str(&body).expect("info json");
    let advertised = info.get("relay_url").and_then(Value::as_str).unwrap_or("");
    assert!(
        !advertised.is_empty(),
        "expected non-empty relay_url in /info, got {info}",
    );

    let mut chat = lair.chat().await.expect("open chat ws");
    chat.send_user_message("hi").await.expect("send");
    chat.collect_turn().await.expect("collect turn");

    let tools = captured_tool_names(&lair);
    assert!(
        tools.iter().any(|n| n == "send_notification"),
        "send_notification should be registered when push is enabled; tools={tools:?}",
    );
    assert!(
        tools.iter().any(|n| n == "ask_question"),
        "ask_question should be registered when push is enabled; tools={tools:?}",
    );
}
