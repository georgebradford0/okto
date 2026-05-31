//! `okto agents …`. `list` reads the on-disk registry directly; `start` /
//! `stop` / `delete` drive lair's management API, which we stand in for with a
//! `MockMgmt` server pointed to by a synthetic `lair-launch.json`.

mod common;

use common::{MockMgmt, OktoCli};
use serde_json::json;

/// A minimal but schema-valid `agents.json` with one local, running agent.
fn registry_with_one_agent() -> String {
    json!({
        "agents": [{
            "name": "worker",
            "pid": 4242,
            "port": 30100,
            "status": "running",
            "binary_version": "0.20.0",
            "created_at": 1_700_000_000u64,
            "last_seen": 1_700_000_000u64
        }]
    })
    .to_string()
}

#[tokio::test]
async fn list_renders_the_registry() {
    let cli = OktoCli::new();
    cli.write(".okto/lair/agents.json", &registry_with_one_agent());

    let out = cli.run(&["agents", "list"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("NAME"), "missing table header: {}", out.stdout);
    assert!(out.stdout.contains("worker"), "missing agent row: {}", out.stdout);
    assert!(out.stdout.contains("running"), "missing status: {}", out.stdout);
    assert!(out.stdout.contains("local"), "expected a local agent: {}", out.stdout);
}

#[tokio::test]
async fn list_with_no_registry_reports_none() {
    let cli = OktoCli::new();
    let out = cli.run(&["agents", "list"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("No agents"), "{}", out.stdout);
}

#[tokio::test]
async fn start_posts_to_the_management_api() {
    let cli = OktoCli::new();
    let mock = MockMgmt::start().await;
    cli.write_launch(8443, mock.port);

    let out = cli.run(&["agents", "start", "worker"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("Started 'worker'."), "{}", out.stdout);
    assert!(mock.saw("POST", "/agents/worker/start"), "requests: {:?}", mock.requests());
}

#[tokio::test]
async fn stop_posts_to_the_management_api() {
    let cli = OktoCli::new();
    let mock = MockMgmt::start().await;
    cli.write_launch(8443, mock.port);

    let out = cli.run(&["agents", "stop", "worker"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("Stopped 'worker'."), "{}", out.stdout);
    assert!(mock.saw("POST", "/agents/worker/stop"), "requests: {:?}", mock.requests());
}

#[tokio::test]
async fn delete_with_yes_skips_the_prompt_and_calls_delete() {
    let cli = OktoCli::new();
    let mock = MockMgmt::start().await;
    cli.write_launch(8443, mock.port);

    let out = cli.run(&["agents", "delete", "worker", "--yes"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("Deleted 'worker'."), "{}", out.stdout);
    assert!(mock.saw("DELETE", "/agents/worker"), "requests: {:?}", mock.requests());
}

#[tokio::test]
async fn start_forwards_the_management_token_when_present() {
    let cli = OktoCli::new();
    let mock = MockMgmt::start().await;
    cli.write_launch(8443, mock.port);
    cli.write(".okto/lair/.mgmt-token", "deadbeefcafef00d");

    cli.run(&["agents", "start", "worker"]).await.assert_ok();
    let reqs = mock.requests();
    let start = reqs.iter().find(|r| r.path == "/agents/worker/start").expect("start request");
    assert_eq!(
        start.token.as_deref(),
        Some("deadbeefcafef00d"),
        "expected the X-Okto-Token header to be forwarded; reqs: {reqs:?}",
    );
}

#[tokio::test]
async fn start_surfaces_a_lair_error_status() {
    let cli = OktoCli::new();
    let mock = MockMgmt::start_with(500, json!({"error": "boom"})).await;
    cli.write_launch(8443, mock.port);

    let out = cli.run(&["agents", "start", "worker"]).await;
    out.assert_err();
    assert!(
        out.stderr.contains("500"),
        "expected the upstream status in the error, got: {}",
        out.stderr,
    );
}
