//! Phase 3 — tool execution end to end. The mock model returns a `tool_use`
//! for the builtin `bash` tool; lair runs it for real (observable filesystem
//! side effect), feeds the result back, and the scripted follow-up text closes
//! the turn.

mod common;

use std::time::Duration;

use common::{event_types, LairProcess, Turn};

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
