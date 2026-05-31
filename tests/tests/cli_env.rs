//! `okto env …` — manages `~/.okto/lair-env`. `show` and the validation paths
//! run without docker; `set`/`unset` only reach the container restart *after*
//! validation, so the rejection cases are fully offline.

mod common;

use common::OktoCli;

#[tokio::test]
async fn show_on_a_fresh_home_reports_no_vars() {
    let cli = OktoCli::new();
    let out = cli.run(&["env", "show"]).await;
    out.assert_ok();
    assert!(
        out.stdout.contains("no operator env vars set"),
        "expected the empty-state hint, got: {}",
        out.stdout,
    );
}

#[tokio::test]
async fn show_masks_values_and_hides_managed_keys() {
    let cli = OktoCli::new();
    // Seed a lair-env with an operator var and a managed var.
    cli.write(".okto/lair-env", "GH_TOKEN=ghp_supersecrettoken123\nHOME=/data\n");

    let out = cli.run(&["env", "show"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("GH_TOKEN="), "operator key missing: {}", out.stdout);
    assert!(
        !out.stdout.contains("ghp_supersecrettoken123"),
        "value should be masked, not printed in full: {}",
        out.stdout,
    );
    // Managed keys are filtered out of the operator view.
    assert!(!out.stdout.contains("HOME="), "managed key leaked into env show: {}", out.stdout);
}

#[tokio::test]
async fn set_rejects_a_reserved_managed_key() {
    let cli = OktoCli::new();
    let out = cli.run(&["env", "set", "NOISE_PORT=9999"]).await;
    out.assert_err();
    assert!(
        out.stderr.contains("reserved") || out.stderr.contains("managed"),
        "expected a reserved-key error, got: {}",
        out.stderr,
    );
    // Nothing should have been written.
    assert!(!cli.read(".okto/lair-env").contains("NOISE_PORT"), "env file mutated despite rejection");
}

#[tokio::test]
async fn set_rejects_malformed_pairs() {
    let cli = OktoCli::new();
    let out = cli.run(&["env", "set", "NOT_A_PAIR"]).await;
    out.assert_err();
    assert!(
        out.stderr.contains("KEY=VALUE"),
        "expected a KEY=VALUE format error, got: {}",
        out.stderr,
    );
}

#[tokio::test]
async fn unset_refuses_managed_keys() {
    let cli = OktoCli::new();
    let out = cli.run(&["env", "unset", "HOME"]).await;
    out.assert_err();
    assert!(
        out.stderr.contains("managed") || out.stderr.contains("can't be unset"),
        "expected a managed-key error, got: {}",
        out.stderr,
    );
}
