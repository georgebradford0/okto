# Changelog — okto CLI

Changes to the `okto` CLI. Current version: see `cli/Cargo.toml`.

Changelog tracking for the CLI starts here (at 0.6.11). Earlier history lives in
the git log.

## [Unreleased]

### Added
- **`okto reload --check-config`** validates your configuration instead of
  restarting. It checks the effective config values (`~/.okto/config.json`
  overlaid with the matching `~/.okto/lair-env` overrides — `ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`, `MODEL`, `OPENAI_API_URL`, `ANTHROPIC_API_URL`), then sends a
  minimal one-token "ping" turn to the configured backend (Anthropic or
  OpenAI-compatible) to confirm the key, model, and URL actually work. Exits
  non-zero on the first problem and restarts nothing.

## [0.7.3] - 2026-06-04

### Fixed
- **`okto lair update` reliably restarts local agents on the new image.** The
  post-update respawn now addresses each agent by its route-safe `slug` (the key
  lair's management API uses) instead of its display `name`. Previously it passed
  the name and relied on a second registry load to translate it — which fails
  against a slug-keyed lair when the two differ (e.g. an agent named
  `Callos Repo` with slug `callos-repo` returned `400 ... not found`). Note the
  fix must be installed on the host CLI to take effect; an older CLI paired with
  a newer lair image still exhibits the bug.

### Changed
- Agent display **name** is decoupled from its route-safe **slug** across the
  CLI: commands that take an agent reference (`agents start/stop/delete`,
  `tasks --agent`, `mcp --agent`, …) accept either the slug or a unique display
  name and resolve it against the on-disk registry.

## [0.7.2] - 2026-05-31

### Added
- `okto config set --cost-input1m <USD> --cost-output1m <USD>` sets the
  per-1M-token input/output prices (config keys `cost_input1M` /
  `cost_output1M`) used to compute per-turn cost on OpenAI-compatible
  backends. `okto config show` displays them; pass a negative value to clear
  a rate. Anthropic ignores these and uses its built-in pricing.

## [0.7.1] - 2026-05-31

### Added
- **`--ready-timeout <SECS>` on `okto init` and `okto reload`** to control
  how long the CLI waits for `/health` after `docker run` / `docker restart`.
  Defaults to **180s** (up from the previous hard-coded 60s) so containers
  with heavy `~/.okto/bootstrap.sh` scripts — e.g. apt-installing Proton
  Bridge (~216 MB) — don't trip a spurious "lair did not become ready" on
  fresh image pulls when the apt cache is cold. `okto lair update`,
  `okto env set`, and `okto env unset` also pick up the 180s default.

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
