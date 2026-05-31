//! `okto tasks …`. `list` reads `tasks.json` files directly; `stop` POSTs to
//! lair's management API (stood in for by `MockMgmt`).

mod common;

use common::{MockMgmt, OktoCli};
use serde_json::json;

/// A `tasks.json` matching the fields the CLI's `TaskRow` deserializes.
fn tasks_json() -> String {
    json!([{
        "task_id": "bg-abc12345",
        "command": "cargo build --release",
        "status": "running",
        "started_at": 1_700_000_000u64,
        "completed_at": null
    }])
    .to_string()
}

#[tokio::test]
async fn list_empty_reports_no_tasks() {
    let cli = OktoCli::new();
    let out = cli.run(&["tasks", "list"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("No tasks."), "{}", out.stdout);
}

#[tokio::test]
async fn list_renders_lair_tasks() {
    let cli = OktoCli::new();
    cli.write(".okto/lair/session/tasks.json", &tasks_json());

    let out = cli.run(&["tasks", "list"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("TASK ID"), "missing header: {}", out.stdout);
    assert!(out.stdout.contains("bg-abc12345"), "missing task id: {}", out.stdout);
    assert!(out.stdout.contains("cargo build"), "missing command: {}", out.stdout);
}

#[tokio::test]
async fn list_for_a_named_agent_reads_its_file() {
    let cli = OktoCli::new();
    cli.write(".okto/agents/worker/data/session/tasks.json", &tasks_json());

    let out = cli.run(&["tasks", "list", "--agent", "worker"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("bg-abc12345"), "{}", out.stdout);
}

#[tokio::test]
async fn list_for_a_named_agent_with_no_tasks() {
    let cli = OktoCli::new();
    let out = cli.run(&["tasks", "list", "--agent", "ghost"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("No tasks for agent 'ghost'."), "{}", out.stdout);
}

#[tokio::test]
async fn stop_reports_fired_when_lair_cancels() {
    let cli = OktoCli::new();
    let mock = MockMgmt::start_with(200, json!({"id": "bg-abc12345", "fired": true})).await;
    cli.write_launch(8443, mock.port);

    let out = cli.run(&["tasks", "stop", "bg-abc12345"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("Stopped task 'bg-abc12345'."), "{}", out.stdout);
    assert!(mock.saw("POST", "/tasks/bg-abc12345/cancel"), "requests: {:?}", mock.requests());
}

#[tokio::test]
async fn stop_reports_not_running_when_not_fired() {
    let cli = OktoCli::new();
    let mock = MockMgmt::start_with(200, json!({"fired": false})).await;
    cli.write_launch(8443, mock.port);

    let out = cli.run(&["tasks", "stop", "bg-missing"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("not running"), "{}", out.stdout);
}

#[tokio::test]
async fn stop_targets_the_agent_proxy_path() {
    let cli = OktoCli::new();
    let mock = MockMgmt::start_with(200, json!({"fired": true})).await;
    cli.write_launch(8443, mock.port);

    cli.run(&["tasks", "stop", "bg-abc12345", "--agent", "worker"]).await.assert_ok();
    assert!(
        mock.saw("POST", "/agents/worker/tasks/bg-abc12345/cancel"),
        "requests: {:?}",
        mock.requests(),
    );
}
