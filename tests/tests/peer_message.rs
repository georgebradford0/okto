//! Inter-agent peer messaging — the lair→agent injection path.
//!
//! Drives a real `lair --role agent` over plaintext loopback, POSTs a peer
//! message to its `/inject` endpoint exactly as lair would when its model calls
//! `send_message_to_agent`, and asserts that:
//!   1. the agent auto-starts a turn to act on the message (its model is woken),
//!   2. the message is persisted into the agent's history as a `peer_message`
//!      row so the model sees it as conversation input.
//!
//! This exercises the shared injection mechanism (`pending_injections` +
//! `try_continue_auto` + the `peer_message` role) that both directions rely on.

mod common;

use common::{AgentProcess, Turn};
use serde_json::json;

#[tokio::test]
async fn injected_peer_message_wakes_the_agent() {
    // Agent with a single scripted reply; the workspace need not be a repo.
    let agent = AgentProcess::start_without_repo(vec![Turn::text("Acknowledged — on it.")])
        .await
        .expect("agent to start");

    // No turn has run yet.
    assert_eq!(agent.mock.request_count(), 0, "no model call before inject");

    // Lair delivers a peer message into the agent's main chat.
    let (status, body) = agent
        .http_post_json("/inject", &json!({ "from": "lair", "text": "summarize the repo" }))
        .await
        .expect("POST /inject");
    assert_eq!(status, 200, "inject status; body={body}");

    // The injection should auto-start a turn. Poll /history until the model's
    // scripted reply lands (proving it was woken to act on the message).
    let mut history = String::new();
    let mut woke = false;
    for _ in 0..100 {
        let (s, hist) = agent.http_get("/history").await.expect("GET /history");
        assert_eq!(s, 200);
        history = hist;
        if history.contains("Acknowledged") {
            woke = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(woke, "agent did not auto-turn on the injected message\n--- history ---\n{history}\n--- log ---\n{}", agent.log());

    // The model was actually called by the auto-turn.
    assert!(agent.mock.request_count() >= 1, "mock LLM should have served a turn");

    // History persists the injection as a `peer_message` row carrying the text,
    // so a /history reload (or restart) replays it into the conversation.
    assert!(history.contains("peer_message"), "history missing peer_message role:\n{history}");
    assert!(history.contains("summarize the repo"), "history missing injected text:\n{history}");
    assert!(history.contains("message from lair"), "history missing sender prefix:\n{history}");
}
