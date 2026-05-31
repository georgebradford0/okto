# Changelog — okto CLI

Changes to the `okto` CLI. Current version: see `cli/Cargo.toml`.

Changelog tracking for the CLI starts here (at 0.6.11). Earlier history lives in
the git log.

## [Unreleased]

## [0.7.0] - 2026-05-31

### Added
- **`okto init --disable-push`.** Persists `OKTO_RELAY_URL=` (explicit empty)
  into `~/.okto/lair-env`, which silences push notifications end-to-end: lair
  and child agents drop the `send_notification` and `ask_question` tools from
  the LLM's tool list, and the mobile client skips registering for pushes
  because lair's `/info` advertises an empty relay URL. To re-enable later,
  `okto env unset OKTO_RELAY_URL && okto reload`. Rejected if combined with
  `--env OKTO_RELAY_URL=...`.
- Black-box e2e test suite for the `okto` CLI (in the `okto-tests` crate):
  spawns the real binary against a temp `HOME` and asserts stdout/stderr/exit
  code plus on-disk `~/.okto` state. Covers `version`, `completions`, `config`,
  `env`, `mcp list/remove`, `qr`, `ssh pubkey`, `agents`, and `tasks`. Commands
  that hit lair's loopback management API are driven against an in-process mock,
  so the suite is fully offline (no docker, no network). Run with
  `cargo test -p okto-tests`.
