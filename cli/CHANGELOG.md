# Changelog — okto CLI

Changes to the `okto` CLI. Current version: see `cli/Cargo.toml`.

Changelog tracking for the CLI starts here (at 0.6.11). Earlier history lives in
the git log.

## [Unreleased]

### Added
- Black-box e2e test suite for the `okto` CLI (in the `okto-tests` crate):
  spawns the real binary against a temp `HOME` and asserts stdout/stderr/exit
  code plus on-disk `~/.okto` state. Covers `version`, `completions`, `config`,
  `env`, `mcp list/remove`, `qr`, `ssh pubkey`, `agents`, and `tasks`. Commands
  that hit lair's loopback management API are driven against an in-process mock,
  so the suite is fully offline (no docker, no network). Run with
  `cargo test -p okto-tests`.
