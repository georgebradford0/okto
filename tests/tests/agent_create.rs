//! Create-agent naming: when `POST /agents` omits `name`, lair asks the model
//! to name the child from its spawn context (a single tool-less model call);
//! when `name` is given, it's used verbatim and no naming call is made.
//!
//! These drive the real lair binary over the Noise tunnel like the mobile
//! client. Under the non-root test harness the child's privilege drop
//! (`cmd.uid()/gid()`) EPERMs, so the actual spawn fails — but the chosen name
//! is decided *before* the spawn and is echoed back in lair's response in every
//! outcome (success message or spawn/health error), which is what we assert on.
//! `OKTO_CREATE_AGENT_TIMEOUT_SECS` is pinned low so a spawn that does succeed
//! (e.g. a root CI container) doesn't block the request on the health wait.

mod common;

use common::{LairProcess, Turn};

/// Pin the post-spawn health wait short so the request returns promptly
/// regardless of whether the child boots.
const FAST_CREATE: &[(&str, &str)] = &[("OKTO_CREATE_AGENT_TIMEOUT_SECS", "5")];

#[tokio::test]
async fn omitting_name_uses_model_generated_name() {
    // The first (and only) model call is the name request; the mock returns a
    // human-style name that lair must slugify into a registry-safe slug.
    let lair = LairProcess::start_with_env(vec![Turn::text("Auth Refactor Otter")], FAST_CREATE)
        .await
        .expect("lair to start");

    let (status, body) = lair
        .http_post_json("/agents", r#"{"startup_prompt":"refactor the auth module"}"#)
        .await
        .expect("POST /agents");

    // The model was asked exactly once — for the name.
    assert_eq!(lair.mock.request_count(), 1, "expected one naming model call");

    // That call carried the naming system prompt and the spawn context.
    let req = &lair.mock.requests()[0];
    let system = serde_json::to_string(&req["system"]).unwrap();
    assert!(system.contains("name coding agents"), "naming system prompt missing: {system}");
    let messages = serde_json::to_string(&req["messages"]).unwrap();
    assert!(messages.contains("refactor the auth module"), "startup prompt not in context: {messages}");

    // The slugified model name ("auth-refactor-otter") is the child's name, and
    // shows up in lair's response whether the spawn succeeded or failed.
    assert!(
        body.contains("auth-refactor-otter"),
        "expected model-derived name in response (status {status}): {body}",
    );
    assert!(
        !body.contains("lair-workload"),
        "should not have fallen back to the default name: {body}",
    );

    // Best-effort cleanup in case the child actually spawned (root host).
    let _ = lair.http_post("/agents/auth-refactor-otter/stop").await;
}

#[tokio::test]
async fn explicit_name_skips_the_model_call() {
    let lair = LairProcess::start_with_env(vec![Turn::text("should-not-be-used")], FAST_CREATE)
        .await
        .expect("lair to start");

    let (status, body) = lair
        .http_post_json("/agents", r#"{"name":"my-exact-agent"}"#)
        .await
        .expect("POST /agents");

    // No naming call — the explicit name is used verbatim.
    assert_eq!(lair.mock.request_count(), 0, "explicit name must not trigger a model call");
    assert!(
        body.contains("my-exact-agent"),
        "expected explicit name in response (status {status}): {body}",
    );

    let _ = lair.http_post("/agents/my-exact-agent/stop").await;
}

#[tokio::test]
async fn explicit_name_with_spaces_is_slugged() {
    // A free-form display name with spaces must be turned into a route-safe
    // slug for the on-disk dir and the wire id, while the display name itself
    // is kept verbatim.
    let lair = LairProcess::start_with_env(vec![Turn::text("should-not-be-used")], FAST_CREATE)
        .await
        .expect("lair to start");

    let (status, body) = lair
        .http_post_json("/agents", r#"{"name":"My Cool Agent"}"#)
        .await
        .expect("POST /agents");

    // Explicit name → no naming model call.
    assert_eq!(lair.mock.request_count(), 0, "explicit name must not trigger a model call");
    // The raw spaced name is preserved as the display label in the response.
    assert!(
        body.contains("My Cool Agent"),
        "expected display name preserved in response (status {status}): {body}",
    );

    // The per-agent dir is created (before the privilege-drop spawn, which
    // EPERMs under the non-root harness) at the *slug*, never the spaced name.
    let agents_root = lair.home.join("agents");
    assert!(
        agents_root.join("my-cool-agent").is_dir(),
        "expected the on-disk dir to be the slug 'my-cool-agent' under {}",
        agents_root.display(),
    );
    assert!(
        !agents_root.join("My Cool Agent").exists(),
        "the raw spaced name must never become a directory under {}",
        agents_root.display(),
    );

    // Best-effort cleanup in case the child actually spawned (root host) —
    // addressed by slug, never the spaced name.
    let _ = lair.http_post("/agents/my-cool-agent/stop").await;
}
