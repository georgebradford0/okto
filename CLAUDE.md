# octo — project notes for Claude

`@` is used as a path prefix to reference files in the repository (e.g. `@src/main.rs`).

## Git workflow

Do **not** create git worktrees unless explicitly asked to. Commit and push directly on the current branch.

Do **not** commit debug/diagnostic logging (`println!`, `console.log`, etc. added purely for investigation). Suggest the user add logs locally instead.

## Platform

Linux only — x86_64 and aarch64. macOS and Windows are out of scope. Both the `octo` CLI and the `octo-lair` server are built for the two Linux targets; the cloud-init userdata `mint_bootstrap_userdata` emits is also Linux.

CI publishes per-arch binaries for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`. `scripts/get-cli.sh` auto-detects and downloads the right pair.

## Binaries

One Rust binary, `octo-lair`, two roles via `--role lair|agent`. There is no Docker image and no shell entrypoint script.

- Operator runs `octo init`; the CLI spawns `octo-lair --role lair` as a detached background process. The pid is recorded at `~/.octo/lair/lair.pid`.
- When lair creates a child agent, it spawns `octo-lair --role agent` itself (via `tokio::process::Command`), recording the child's pid in `~/.octo/lair/agents.json`.
- `lair/src/bootstrap.rs` does the public-IP detection, optional git clone, and `STARTUP_SCRIPT` execution in Rust.

Build for release:

```sh
cargo build --release -p octo-lair
cargo build --release -p octo
```

Both binaries end up at `target/release/{octo-lair,octo}`. CI publishes them per-target as GitHub Release artefacts.

---

## Architecture overview

octo is an agentic coding assistant. A single `lair` process runs on a host machine, supervises child agent processes via a small `AgentSupervisor`, and exposes itself to a mobile client over an encrypted Noise tunnel. Mobile traffic to child agents is proxied through lair — children never get a public network surface.

### Components

| Directory | Language | Role |
|-----------|----------|------|
| `core/` | Rust | Shared library: agentic loop, Claude API streaming, git ops, config, HTTP/WS plumbing, agent registry, SSH keygen, MCP plumbing, Noise proxy. |
| `lair/` | Rust + Axum | Merged binary `octo-lair` with `--role lair|agent`. `lair/src/lair.rs` is the parent (orchestrates child processes via `lair/src/agent_proc.rs`); `lair/src/agent.rs` is the child. |
| `cli/` | Rust | `octo` CLI for managing lair on the local host (init, reload, agents, logs, mcp, config, env). |
| `mobile/` | React Native (TS) | iOS/Android client: QR scan → native Noise tunnel → single WebSocket to lair → optional proxy URL for chatting with children. |

### Filesystem layout

Everything lives under `~/.octo/`:

- `~/.octo/config.json` — operator credentials (API keys, model). Read by every role via `octo_core::config_path()`.
- `~/.octo/lair-env` — extra env vars passed to lair (operator-managed via `octo env`).
- `~/.octo/lair-launch.json` — port bookkeeping for `octo reload`.
- `~/.octo/lair/` — lair's per-process data dir (`OCTO_DATA_DIR`). Holds `lair.pid`, `lair.log`, `noise_key.bin`, `agents.json`, `mcp.json`, `messages.json`, `tasks.json`, `relay_signing_key.bin`, `ssh_id_ed25519{,.pub}`, `known_hosts`.
- `~/.octo/agents/<name>/` — per-agent dirs. Each has `data/` (the agent's `OCTO_DATA_DIR`) and `workspace/` (its `WORKSPACE_DIR`), plus an `agent.log` capture.

### Agent registry

`~/.octo/lair/agents.json` is lair's single source of truth for which agents exist. Schema is `octo_core::AgentRecord`:

```
{ name, pid, port, git_url, status, binary_version, created_at, last_seen }
```

`pid` is the recorded OS pid of the last spawned `octo-lair --role agent` process. The poller checks `kill(pid, 0)` every 10s and flips `status` accordingly. On lair startup, the supervisor adopts any rows whose pid is still alive.

### Transport

Mobile ↔ lair is encrypted with **Noise_XX_25519_ChaChaPoly_SHA256**:

1. Client scans QR code → `2:<host>:<port>:<base32-pubkey>`.
2. TCP connection → 3-message Noise XX handshake.
3. Frame format: 2-byte big-endian length prefix + ciphertext. Frames over `MAX_FRAME_SIZE` (16 KiB) are rejected.
4. Post-handshake: WebSockets run over the encrypted frames.
5. **Children are *never* reached directly.** Mobile opens a WebSocket to `ws://lair/agents/<name>/stream` over the same tunnel; lair proxies frames bidirectionally to the child's loopback HTTP port via `tokio_tungstenite`.

Wire frames (see `core/src/lib.rs::ChatEvent` and `mobile/src/wire.ts`):

| `type`            | Direction | Payload |
|-------------------|-----------|---------|
| `ready`           | s → c     | `session_id`, `resumed`. Sent on each WS open. |
| `text`            | s → c     | Streamed model text deltas. |
| `tool_use` / `tool_output` / `tool_result` | s → c | Tool invocation lifecycle. |
| `done` / `interrupted` / `error` | s → c | Turn terminators. |
| `agents`          | s → c     | **Lair only.** Pushed on poller change. |
| `tasks` / `bg_complete` | s → c | Per-chat background-task registry. |
| `ping` / `pong`   | both      | Keepalive (15 s interval, 2 missed = evict). |
| `user_message`    | c → s     | Start a new agentic turn. |
| `interrupt`       | c → s     | Cancel the in-flight turn. |
| `start_agent` / `terminate_agent` | c → s | **Lair only.** Lifecycle ops. |
| `cancel_task`     | c → s     | Cancel a running background task. |

Lair listens on `NOISE_PORT` (default 8443 in prod) and forwards Noise traffic to its own HTTP server on `127.0.0.1:8000`.

### Lair management API (CLI ↔ lair on loopback)

Lair exposes a small management API on `127.0.0.1:8000` for the CLI:

- `GET    /agents` — list registry rows.
- `POST   /agents` — `{name?, git_url?, port?, startup_script?, startup_prompt?}` — spawn a new child.
- `POST   /agents/:name/start` / `stop` — supervisor control.
- `DELETE /agents/:name` — terminate + remove data dir.
- `GET    /agents/:name/logs` — last 1 MB of the child's `agent.log`.
- `GET    /agents/:name/stream` — WebSocket proxy (mobile end).
- `GET    /agents/:name/history`, `POST /agents/:name/interrupt`, `POST /agents/:name/clear`, `GET /agents/:name/branches` — HTTP proxies of the child's same-name endpoints.

### lair credentials (`~/.octo/config.json`)

Lair reads its API keys and provider settings from `~/.octo/config.json`. Lair re-reads it on every model call, so rotation is live. Children inherit credentials via env at spawn time.

`GH_TOKEN` lives in `~/.octo/lair-env` (operator-managed via `octo env set GH_TOKEN=…`).

### lair runtime env (non-secret, set by the CLI)

| Variable | Purpose |
|----------|---------|
| `OCTO_DATA_DIR` | `~/.octo/lair` |
| `OCTO_AGENTS_DIR` | `~/.octo/agents` |
| `OCTO_LAIR_BINARY` | Path to `octo-lair` (used to spawn children) |
| `NOISE_PORT` / `PUBLIC_PORT` | Noise listen / advertised port |
| `OCTO_SKIP_SHELL_ENV` | Always set to 1; suppresses the login-shell env sourcing |

### agent (child) env (set by lair on spawn)

| Variable | Purpose |
|----------|---------|
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `MODEL` / `OPENAI_API_URL` | Provider credentials |
| `AGENT_PORT` | Loopback HTTP port (30100–30199) |
| `OCTO_DATA_DIR` | `~/.octo/agents/<name>/data` |
| `WORKSPACE_DIR` | `~/.octo/agents/<name>/workspace` |
| `GIT_URL` | Optional repo to clone (HTTPS needs `GH_TOKEN`) |
| `AGENT_PURPOSE` / `STARTUP_SCRIPT` / `STARTUP_PROMPT` | Optional bootstrap knobs |

### CI/CD workflows (all manual dispatch)

| Workflow | What it does |
|----------|--------------|
| `cli.yml` | Builds `octo` and `octo-lair` per-target and uploads as Release assets. |
| `android.yml` | Builds AAB via fastlane, uploads to Google Play. |
| `ios.yml` | Builds on macOS runner, optionally uploads to TestFlight. |
