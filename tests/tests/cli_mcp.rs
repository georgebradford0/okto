//! `okto mcp list` / `okto mcp remove` — operate directly on the per-process
//! `mcp.json` files, so they run without a lair container. (`mcp add` /
//! `mcp import` require a running container to verify connections and are out
//! of scope for offline e2e.)

mod common;

use common::OktoCli;

const TWO_SERVERS: &str = r#"[
  { "name": "fs",      "command": "mcp-fs",   "args": ["--root", "/tmp"], "env": {} },
  { "name": "fetcher", "command": "mcp-fetch", "args": [],                "env": {} }
]"#;

#[tokio::test]
async fn list_empty_for_lair_by_default() {
    let cli = OktoCli::new();
    let out = cli.run(&["mcp", "list"]).await;
    out.assert_ok();
    assert!(
        out.stdout.contains("No MCP servers configured in 'lair'."),
        "{}",
        out.stdout,
    );
}

#[tokio::test]
async fn list_renders_configured_lair_servers() {
    let cli = OktoCli::new();
    cli.write(".okto/lair/mcp.json", TWO_SERVERS);

    let out = cli.run(&["mcp", "list"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("fs: mcp-fs --root /tmp"), "{}", out.stdout);
    assert!(out.stdout.contains("fetcher: mcp-fetch"), "{}", out.stdout);
}

#[tokio::test]
async fn list_reads_a_named_agents_config() {
    let cli = OktoCli::new();
    cli.write(".okto/agents/worker/data/mcp.json", TWO_SERVERS);

    let out = cli.run(&["mcp", "list", "--agent", "worker"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("fs: mcp-fs"), "{}", out.stdout);
}

#[tokio::test]
async fn remove_drops_a_server_from_the_file() {
    let cli = OktoCli::new();
    cli.write(".okto/lair/mcp.json", TWO_SERVERS);

    let out = cli.run(&["mcp", "remove", "--agent", "lair", "fs"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("Removed MCP server 'fs' from 'lair'."), "{}", out.stdout);

    // The file should now hold only the other server.
    let remaining = cli.read(".okto/lair/mcp.json");
    assert!(remaining.contains("fetcher"), "fetcher should survive removal: {remaining}");
    assert!(!remaining.contains("\"fs\""), "fs should be gone: {remaining}");
}

#[tokio::test]
async fn remove_unknown_server_errors() {
    let cli = OktoCli::new();
    cli.write(".okto/lair/mcp.json", TWO_SERVERS);

    let out = cli.run(&["mcp", "remove", "--agent", "lair", "nope"]).await;
    out.assert_err();
    assert!(out.stderr.contains("not found"), "expected a not-found error, got: {}", out.stderr);
}
