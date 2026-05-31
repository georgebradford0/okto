//! `okto config show` / `okto config set` — pure local reads/writes of
//! `~/.okto/config.json`. No docker, no lair container.

mod common;

use common::OktoCli;

#[tokio::test]
async fn show_on_a_fresh_home_reports_unset_defaults() {
    let cli = OktoCli::new();
    let out = cli.run(&["config", "show"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("anthropic_api_key:    (not set)"), "{}", out.stdout);
    assert!(out.stdout.contains("openai_api_key:       (not set)"), "{}", out.stdout);
    assert!(out.stdout.contains("model:                (default)"), "{}", out.stdout);
    assert!(out.stdout.contains("api_url:              (Anthropic)"), "{}", out.stdout);
    assert!(out.stdout.contains("cost_input1M:         (not set)"), "{}", out.stdout);
    assert!(out.stdout.contains("cost_output1M:        (not set)"), "{}", out.stdout);
    assert!(out.stdout.contains("system_prompt_append: (not set)"), "{}", out.stdout);
}

#[tokio::test]
async fn set_cost_rates_persist_and_clear() {
    let cli = OktoCli::new();

    cli.run(&["config", "set", "--cost-input1m", "2.5", "--cost-output1m", "10"])
        .await
        .assert_ok();

    // Persisted to disk under the serde-renamed JSON keys.
    let on_disk = cli.read(".okto/config.json");
    assert!(on_disk.contains("cost_input1M"), "config.json should hold cost_input1M: {on_disk}");
    assert!(on_disk.contains("cost_output1M"), "config.json should hold cost_output1M: {on_disk}");

    let show = cli.run(&["config", "show"]).await;
    show.assert_ok();
    assert!(show.stdout.contains("cost_input1M:         $2.5/1M"), "{}", show.stdout);
    assert!(show.stdout.contains("cost_output1M:        $10/1M"), "{}", show.stdout);

    // A negative value clears a rate (no other unset mechanism for a number).
    cli.run(&["config", "set", "--cost-input1m", "-1"]).await.assert_ok();
    let cleared = cli.run(&["config", "show"]).await;
    assert!(cleared.stdout.contains("cost_input1M:         (not set)"), "{}", cleared.stdout);
    // The output rate set earlier is untouched.
    assert!(cleared.stdout.contains("cost_output1M:        $10/1M"), "{}", cleared.stdout);
}

#[tokio::test]
async fn set_then_show_persists_and_masks() {
    let cli = OktoCli::new();

    let key = "sk-ant-abcdefgh12345678";
    let set = cli
        .run(&["config", "set", "--model", "claude-test", "--anthropic-api-key", key])
        .await;
    set.assert_ok();
    assert!(set.stdout.contains("Config updated."), "{}", set.stdout);

    // The full key is written verbatim to disk...
    let on_disk = cli.read(".okto/config.json");
    assert!(on_disk.contains(key), "config.json should hold the raw key: {on_disk}");
    assert!(on_disk.contains("claude-test"), "config.json should hold the model: {on_disk}");

    // ...but `show` masks it (first 4 + last 4) and never prints it in full.
    let show = cli.run(&["config", "show"]).await;
    show.assert_ok();
    assert!(show.stdout.contains("sk-a"), "masked prefix missing: {}", show.stdout);
    assert!(show.stdout.contains("5678"), "masked suffix missing: {}", show.stdout);
    assert!(show.stdout.contains("..."), "mask separator missing: {}", show.stdout);
    assert!(!show.stdout.contains(key), "show leaked the raw key: {}", show.stdout);
    assert!(show.stdout.contains("model:                claude-test"), "{}", show.stdout);
}

#[tokio::test]
async fn set_is_additive_across_invocations() {
    let cli = OktoCli::new();
    cli.run(&["config", "set", "--model", "m1"]).await.assert_ok();
    cli.run(&["config", "set", "--api-url", "https://example.test/v1/chat/completions"])
        .await
        .assert_ok();

    let show = cli.run(&["config", "show"]).await;
    show.assert_ok();
    // The second set must not have clobbered the model from the first.
    assert!(show.stdout.contains("model:                m1"), "{}", show.stdout);
    assert!(
        show.stdout.contains("api_url:              https://example.test/v1/chat/completions"),
        "{}",
        show.stdout,
    );
}

#[tokio::test]
async fn set_system_prompt_append_can_be_cleared() {
    let cli = OktoCli::new();
    cli.run(&["config", "set", "--system-prompt-append", "be terse"]).await.assert_ok();
    let shown = cli.run(&["config", "show"]).await;
    assert!(shown.stdout.contains("system_prompt_append: be terse"), "{}", shown.stdout);

    // Passing an empty string clears it.
    cli.run(&["config", "set", "--system-prompt-append", ""]).await.assert_ok();
    let cleared = cli.run(&["config", "show"]).await;
    assert!(
        cleared.stdout.contains("system_prompt_append: (not set)"),
        "expected the append to be cleared: {}",
        cleared.stdout,
    );
}
