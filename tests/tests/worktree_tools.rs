//! Agent worktree tools end to end — `create_worktree` / `remove_worktree`.
//!
//! These tools are offered to a child **agent** (not lair) only when a git repo
//! is attached to its workspace. The mock model returns a `tool_use`; the agent
//! drives its own `/worktrees` HTTP handlers, so the observable proof is the
//! real git worktree on disk plus the agent's `GET /worktrees` list.

mod common;

use common::agent_proc::AgentProcess;
use common::Turn;

/// Pull the offered tool names out of the first request the agent made to the
/// model — the Anthropic `/v1/messages` body carries the `tools` array.
fn offered_tools(agent: &AgentProcess) -> Vec<String> {
    let reqs = agent.mock.requests();
    let first = reqs.first().expect("agent made at least one model request");
    first
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn create_worktree_tool_makes_a_real_worktree() {
    let agent = AgentProcess::start_with_repo(vec![
        Turn::tool("c1", "create_worktree", serde_json::json!({ "branch": "feature/x" })),
        Turn::text("worktree ready"),
    ])
    .await
    .expect("agent to start");

    let mut chat = agent.chat().await.expect("open chat ws");
    chat.wait_ready().await.expect("ready");
    chat.send_user_message("make a worktree for feature/x").await.expect("send");
    let events = chat.collect_turn().await.expect("collect turn");

    let types: Vec<String> = events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(types.contains(&"tool_use".to_string()), "no tool_use in {types:?}");
    assert!(types.contains(&"tool_result".to_string()), "no tool_result in {types:?}");
    assert!(types.contains(&"done".to_string()), "no done in {types:?}");

    let tool_use = events.iter().find(|e| e["type"] == "tool_use").unwrap();
    assert_eq!(tool_use["tool"], "create_worktree");

    // Real side effect: the git worktree exists on disk (its `.git` is a file
    // pointing back at the shared clone) on the requested branch.
    let wt_git = agent.agent_path("worktrees/feature-x/.git");
    let mut found = false;
    for _ in 0..30 {
        if wt_git.exists() {
            found = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(found, "worktree dir not created at {}", wt_git.display());

    // And the agent's authoritative list now carries it.
    let wts = agent.worktrees().await.expect("list worktrees");
    assert_eq!(wts.len(), 1, "expected one worktree, got {wts:?}");
    assert_eq!(wts[0]["id"], "feature-x");
    assert_eq!(wts[0]["branch"], "feature/x");
}

#[tokio::test]
async fn remove_worktree_tool_tears_it_down() {
    // One user turn drives create → remove → closing text (the agentic loop
    // pops one scripted turn per model round-trip).
    let agent = AgentProcess::start_with_repo(vec![
        Turn::tool("c1", "create_worktree", serde_json::json!({ "branch": "wip/cleanup" })),
        Turn::tool("c2", "remove_worktree", serde_json::json!({ "branch": "wip/cleanup" })),
        Turn::text("cleaned up"),
    ])
    .await
    .expect("agent to start");

    let mut chat = agent.chat().await.expect("open chat ws");
    chat.wait_ready().await.expect("ready");
    chat.send_user_message("make then remove a scratch worktree").await.expect("send");
    let events = chat.collect_turn().await.expect("collect turn");

    // Both tool calls ran and the turn closed.
    let tool_names: Vec<String> = events
        .iter()
        .filter(|e| e["type"] == "tool_use")
        .map(|e| e["tool"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(tool_names, vec!["create_worktree", "remove_worktree"], "tool_use sequence: {tool_names:?}");

    // Net effect: no worktree remains, on disk or in the manifest.
    let wts = agent.worktrees().await.expect("list worktrees");
    assert!(wts.is_empty(), "expected no worktrees after removal, got {wts:?}");

    let wt_dir = agent.agent_path("worktrees/wip-cleanup");
    assert!(!wt_dir.exists(), "worktree dir still present at {}", wt_dir.display());
}

#[tokio::test]
async fn worktree_tools_offered_only_with_a_repo() {
    // Repo attached → both tools advertised to the model.
    let with_repo = AgentProcess::start_with_repo(vec![Turn::text("hi")])
        .await
        .expect("agent to start");
    let mut chat = with_repo.chat().await.expect("open chat ws");
    chat.wait_ready().await.expect("ready");
    chat.send_user_message("hello").await.expect("send");
    chat.collect_turn().await.expect("collect turn");
    let tools = offered_tools(&with_repo);
    assert!(tools.contains(&"create_worktree".to_string()), "repo agent missing create_worktree: {tools:?}");
    assert!(tools.contains(&"remove_worktree".to_string()), "repo agent missing remove_worktree: {tools:?}");

    // No repo → neither tool is offered.
    let no_repo = AgentProcess::start_without_repo(vec![Turn::text("hi")])
        .await
        .expect("agent to start");
    let mut chat = no_repo.chat().await.expect("open chat ws");
    chat.wait_ready().await.expect("ready");
    chat.send_user_message("hello").await.expect("send");
    chat.collect_turn().await.expect("collect turn");
    let tools = offered_tools(&no_repo);
    assert!(!tools.contains(&"create_worktree".to_string()), "non-repo agent offered create_worktree: {tools:?}");
    assert!(!tools.contains(&"remove_worktree".to_string()), "non-repo agent offered remove_worktree: {tools:?}");
}
