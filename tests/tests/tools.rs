//! Phase 3 — tool execution end to end. The mock model returns a `tool_use`
//! for the builtin `bash` tool; lair runs it for real (observable filesystem
//! side effect), feeds the result back, and the scripted follow-up text closes
//! the turn.

mod common;

use std::time::Duration;

use common::tunnel::ChatWs;
use common::{event_types, LairProcess, Turn};
use serde_json::Value;

/// Read events until one of `types` is seen; returns that event. Panics if the
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
async fn bash_tool_runs_and_has_a_real_side_effect() {
    // A scratch dir lair will write into via the bash tool (absolute path, so
    // the tool's cwd doesn't matter).
    let scratch = tempfile::tempdir().expect("scratch dir");
    let marker = scratch.path().join("marker.txt");
    let command = format!("echo e2e-tool-ok > {}", marker.display());

    let lair = LairProcess::start(vec![
        Turn::tool("call_1", "bash", serde_json::json!({ "command": command })),
        Turn::text("done writing"),
    ])
    .await
    .expect("lair to start");

    let mut chat = lair.chat().await.expect("open chat ws");
    // consume the `ready` frame
    loop {
        let ev = chat.next_event().await.unwrap().expect("ready");
        if ev["type"] == "ready" {
            break;
        }
    }

    chat.send_user_message("please write the marker")
        .await
        .expect("send");
    let events = chat.collect_turn().await.expect("collect turn");
    let types = event_types(&events);

    // lair's chat loop (`okto_core::send_message`) streams the full tool
    // lifecycle: `tool_use`, any `tool_output` lines during execution, then a
    // `tool_result` frame once the tool finishes — followed by the closing text
    // turn the result unlocked. The filesystem side effect asserted below is the
    // ground-truth proof the tool actually ran.
    assert!(types.contains(&"tool_use".to_string()), "no tool_use in {types:?}");
    assert!(types.contains(&"tool_result".to_string()), "no tool_result in {types:?}");
    assert!(types.contains(&"text".to_string()), "no follow-up text in {types:?}");
    assert!(types.contains(&"done".to_string()), "no done in {types:?}");

    let tool_use = events.iter().find(|e| e["type"] == "tool_use").unwrap();
    assert_eq!(tool_use["tool"], "bash");
    // The streamed tool_use carries the friendly label the mobile client renders
    // in place of the raw tool name.
    assert_eq!(tool_use["display"], "Running command", "tool_use missing friendly display");

    // /history projects the same friendly phrase on the persisted tool row via a
    // `display` field (text stays the raw `name(arg)` for older clients), so a
    // finished tool never reverts to the bare tool name after a reconcile.
    let (status, body) = lair.http_get("/history").await.expect("GET /history");
    assert_eq!(status, 200, "history status: {body}");
    let hist: serde_json::Value = serde_json::from_str(&body).expect("history json");
    let tool_row = hist["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("a tool row in /history");
    assert_eq!(
        tool_row["display"].as_str().unwrap_or_default(),
        "Running command (echo e2e-tool-ok > ".to_string()
            + marker.to_str().unwrap()
            + ")",
        "history tool row missing friendly display; got {tool_row}",
    );

    // The real side effect: the file exists with the content the tool wrote.
    let mut contents = None;
    for _ in 0..30 {
        if let Ok(c) = std::fs::read_to_string(&marker) {
            contents = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let contents = contents.unwrap_or_else(|| panic!("marker file never written: {}", marker.display()));
    assert_eq!(contents.trim(), "e2e-tool-ok");

    // Two model calls: the tool turn, then the follow-up after the tool_result.
    assert_eq!(lair.mock.request_count(), 2, "expected tool turn + follow-up turn");
}

#[tokio::test]
async fn interrupt_kills_the_whole_process_group() {
    // The bash leader backgrounds a child that writes `survivor` after a delay,
    // then blocks. Interrupting must signal the whole process group — the
    // backgrounded child included — so `survivor` is never written. Without a
    // process-group kill, that child orphans to PID 1 and writes the file
    // anyway, which is the regression this guards.
    let scratch = tempfile::tempdir().expect("scratch dir");
    let started  = scratch.path().join("started");
    let survivor = scratch.path().join("survivor");
    let command = format!(
        "echo go > {started}; ( sleep 3; echo alive > {survivor} ) & sleep 30",
        started  = started.display(),
        survivor = survivor.display(),
    );

    let lair = LairProcess::start(vec![
        Turn::tool("call_1", "bash", serde_json::json!({ "command": command })),
        Turn::text("unreached"),
    ])
    .await
    .expect("lair to start");

    let mut chat = lair.chat().await.expect("open chat ws");
    read_until(&mut chat, &["ready"]).await;
    chat.send_user_message("run a backgrounded child").await.expect("send");
    read_until(&mut chat, &["tool_use"]).await;

    // Wait until the command has actually launched its background child.
    for _ in 0..50 {
        if started.exists() { break; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(started.exists(), "command never started");

    chat.interrupt().await.expect("interrupt");
    let terminal = read_until(&mut chat, &["interrupted", "done", "error"]).await;
    assert_eq!(terminal["type"], "interrupted", "expected interrupt, got {terminal}");

    // Wait past the child's 3s delay. With the group killed it can never write;
    // if it orphaned and survived, the file appears within this window.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !survivor.exists(),
        "backgrounded child survived the interrupt and wrote {} — process group not killed",
        survivor.display()
    );
}

#[tokio::test]
async fn interrupted_bash_returns_partial_output() {
    // Output streamed before the kill must reach the model in the tool_result,
    // along with an explicit interrupted marker — so on resume it can tell how
    // far a non-idempotent command got. Output after the interrupt must not.
    let command = "echo first-line; sleep 30; echo second-line";

    let lair = LairProcess::start(vec![
        Turn::tool("call_1", "bash", serde_json::json!({ "command": command })),
        Turn::text("unreached"),
    ])
    .await
    .expect("lair to start");

    let mut chat = lair.chat().await.expect("open chat ws");
    read_until(&mut chat, &["ready"]).await;
    chat.send_user_message("stream then sleep").await.expect("send");
    read_until(&mut chat, &["tool_use"]).await;

    // Make sure the first line was actually streamed before interrupting.
    let out = read_until(&mut chat, &["tool_output"]).await;
    assert!(
        out["line"].as_str().unwrap_or("").contains("first-line"),
        "first tool_output was not first-line: {out}"
    );

    chat.interrupt().await.expect("interrupt");

    // Capture the model-facing tool_result the interrupt produced (it precedes
    // the terminal `interrupted` frame on the same ordered stream).
    let mut tool_result = None;
    loop {
        let ev = chat.next_event().await.expect("ws error").expect("stream closed");
        match ev.get("type").and_then(|x| x.as_str()) {
            Some("tool_result") => tool_result = Some(ev),
            Some("interrupted") | Some("done") | Some("error") => break,
            _ => {}
        }
    }
    let tr = tool_result.expect("a tool_result before the terminal event");
    let output = tr["output"].as_str().unwrap_or("");
    assert!(output.contains("first-line"), "partial output missing first-line: {output:?}");
    assert!(output.contains("[interrupted"), "missing interrupted marker: {output:?}");
    assert!(!output.contains("second-line"), "post-sleep output leaked: {output:?}");
}
