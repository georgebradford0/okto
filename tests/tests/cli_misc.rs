//! CLI smoke tests for argument parsing and the pure-output subcommands
//! (`version`, `completions`) plus clap's error behaviour.

mod common;

use common::OktoCli;

#[tokio::test]
async fn version_prints_a_semver() {
    let cli = OktoCli::new();
    let out = cli.run(&["version"]).await;
    out.assert_ok();
    let v = out.stdout.trim();
    let parts: Vec<&str> = v.split('.').collect();
    assert!(
        parts.len() >= 2 && parts[0].chars().all(|c| c.is_ascii_digit()) && !parts[0].is_empty(),
        "expected a semver-ish version, got {v:?}",
    );
}

#[tokio::test]
async fn completions_bash_mentions_okto() {
    let cli = OktoCli::new();
    let out = cli.run(&["completions", "bash"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("okto"), "bash completion didn't mention okto: {}", out.stdout);
}

#[tokio::test]
async fn completions_zsh_has_compdef_header() {
    let cli = OktoCli::new();
    let out = cli.run(&["completions", "zsh"]).await;
    out.assert_ok();
    assert!(
        out.stdout.contains("#compdef okto"),
        "zsh completion missing #compdef header: {}",
        out.stdout,
    );
}

#[tokio::test]
async fn completions_fish_targets_okto() {
    let cli = OktoCli::new();
    let out = cli.run(&["completions", "fish"]).await;
    out.assert_ok();
    assert!(
        out.stdout.contains("-c okto") || out.stdout.contains("okto"),
        "fish completion didn't target okto: {}",
        out.stdout,
    );
}

#[tokio::test]
async fn no_subcommand_prints_usage_and_fails() {
    let cli = OktoCli::new();
    let out = cli.run(&[]).await;
    out.assert_err();
    assert!(
        out.stderr.contains("Usage") || out.stderr.contains("usage"),
        "expected a usage message on stderr, got: {}",
        out.stderr,
    );
}

#[tokio::test]
async fn unknown_subcommand_is_rejected() {
    let cli = OktoCli::new();
    let out = cli.run(&["frobnicate"]).await;
    out.assert_err();
    assert!(
        out.stderr.contains("frobnicate") || out.stderr.to_lowercase().contains("unrecognized"),
        "expected an unrecognized-subcommand error, got: {}",
        out.stderr,
    );
}

#[tokio::test]
async fn help_flag_succeeds() {
    let cli = OktoCli::new();
    let out = cli.run(&["--help"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("okto"), "help output missing program name: {}", out.stdout);
}
