//! Phase 1 — boot & transport. The real lair binary boots on a temp dir,
//! completes the Noise handshake (verified inside `open_tunnel`), and answers
//! its basic HTTP routes over the encrypted tunnel.

mod common;

use common::LairProcess;

#[tokio::test]
async fn lair_boots_and_health_is_ok() {
    let lair = LairProcess::start(vec![]).await.expect("lair to start");

    // `start` already waited on /health, but assert the body explicitly.
    let (status, body) = lair.http_get("/health").await.expect("GET /health");
    assert_eq!(status, 200, "health status; log:\n{}", lair.log());
    assert!(
        body.contains("ok"),
        "expected an ok-ish health body, got {body:?}"
    );
}

#[tokio::test]
async fn info_route_returns_json() {
    let lair = LairProcess::start(vec![]).await.expect("lair to start");

    let (status, body) = lair.http_get("/info").await.expect("GET /info");
    assert_eq!(status, 200, "info status; log:\n{}", lair.log());
    let v: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("info body not JSON ({e}): {body:?}"));
    assert!(v.is_object(), "expected a JSON object from /info, got {v}");
}

#[tokio::test]
async fn agents_registry_starts_empty() {
    let lair = LairProcess::start(vec![]).await.expect("lair to start");

    let (status, body) = lair.http_get("/agents").await.expect("GET /agents");
    assert_eq!(status, 200, "agents status; log:\n{}", lair.log());
    let v: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("agents body not JSON ({e}): {body:?}"));
    // A freshly-booted lair with no children: the agent list is empty.
    let arr = v.as_array().or_else(|| v.get("agents").and_then(|a| a.as_array()));
    assert_eq!(
        arr.map(|a| a.len()),
        Some(0),
        "expected no agents on a fresh lair, got {v}"
    );
}
