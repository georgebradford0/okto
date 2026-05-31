# okto — project notes for Claude

`@` is used as a path prefix to reference files in the repository (e.g. `@src/main.rs`).

## Git workflow

Do **not** create git worktrees unless explicitly asked to. Commit and push directly on the current branch.

Do **not** commit debug/diagnostic logging (`println!`, `console.log`, etc. added purely for investigation). Suggest the user add logs locally instead.

## Changelogs

There is **no root changelog**. Each app keeps its own `CHANGELOG.md` (Keep-a-Changelog format with an `## [Unreleased]` section at top):

| App | Changelog | Version source of truth |
|-----|-----------|-------------------------|
| `lair/` | `lair/CHANGELOG.md` | `lair/Cargo.toml` |
| `cli/` | `cli/CHANGELOG.md` | `cli/Cargo.toml` |
| `desktop/` | `desktop/CHANGELOG.md` | `desktop/package.json` / `src-tauri/tauri.conf.json` |
| `mobile/` | `mobile/CHANGELOG.md` | `mobile/android/app/build.gradle` (`versionName`) |

When you make a user-facing change to an app, **add an entry under that app's `## [Unreleased]`** in the matching `### Added/Changed/Fixed/Removed/Security` subsection. When you bump an app's version, rename its `## [Unreleased]` heading to `## [<version>] - <YYYY-MM-DD>` and start a fresh empty `## [Unreleased]` above it. A change that spans apps gets an entry in each affected app's changelog.

## Testing

There is **no unit/integration layer by design** — the only tests are **end-to-end** tests that drive the real `lair` binary as a black box. They live in the `tests/` crate (`okto-tests`):

```sh
cargo test -p okto-tests
```

Each test spawns a real `lair --role lair` process on a temp `OKTO_HOME` with ephemeral ports, drives it over the Noise tunnel exactly like the mobile client, and asserts the streamed `ChatEvent` frames + on-disk state. They are **fully offline** — an in-process Anthropic-SSE mock LLM stands in for the real API, so there is **no API spend, no network, and no Docker**. The harness builds the `lair` binary automatically.

- `tests/tests/boot.rs` — boot, Noise handshake, `/health`, `/info`, `/agents`.
- `tests/tests/chat.rs` — a chat turn (`ready → text → done`), history persistence, `/clear`, mid-turn interrupt.
- `tests/tests/tools.rs` — the builtin `bash` tool, asserting the streamed `tool_use`/`tool_result` frames **and** the real filesystem side effect.
- `tests/tests/connection.rs` — lower-level Noise handshake/proxy tests.
- `tests/tests/common/` — the shared harness: `mock_llm.rs` (mock LLM), `lair_proc.rs` (spawns/owns the lair process), `tunnel.rs` (Noise transport + HTTP/WS client).

When you change lair's observable behavior (routes, the agentic loop in `okto_core::send_message`, the wire protocol, transport, or boot sequence), **add or update an e2e test to cover it**. Two prod-only env seams exist purely for these tests and default to production behavior: `OKTO_HTTP_PORT` (lair's loopback HTTP port) and `ANTHROPIC_API_URL` (the Anthropic endpoint in `okto-core`).

**`cargo test -p okto-tests` must pass before cutting a lair image release** (see "Platform").

## Platform

Linux only — x86_64 and aarch64. macOS and Windows are out of scope for the runtime host. The `okto` CLI is built per-Linux-arch and published via the `cli.yml` GitHub Actions workflow; the lair runtime ships exclusively as a multi-arch Docker image (`ghcr.io/georgebradford0/lair`).

Image releases are cut by the **`lair.yml`** GitHub Actions workflow (manual dispatch) after each `lair/Cargo.toml` version bump.

**Prerequisite — the e2e suite must pass first.** Before bumping the version or dispatching `lair.yml`, run the end-to-end tests and confirm they are green (see "Testing"). Do not move forward with the lair release workflow on a red suite:

```sh
cargo test -p okto-tests   # must pass before bumping the version or cutting a release
```

Then dispatch the build:

```sh
gh workflow run lair.yml --ref main
```

`lair.yml` builds each arch **natively** — `linux/amd64` on `ubuntu-latest`, `linux/arm64` on `ubuntu-24.04-arm` — pushes both by digest, then a merge job stitches them into one multi-arch manifest tagged `:<version>` (read from `lair/Cargo.toml`) and `:latest`. Native arm64 runners replace the old QEMU-emulated build that OOMed apt during the arm64 leg.

Building locally is still possible as a fallback (needs `docker login ghcr.io` first):

```sh
docker buildx build \
  --builder multiplatform \
  --platform linux/amd64,linux/arm64 \
  -f lair/Dockerfile \
  -t ghcr.io/georgebradford0/lair:<version> \
  -t ghcr.io/georgebradford0/lair:latest \
  --push .
```

## Binaries & deployment

One Rust binary, `lair`, two roles via `--role lair|agent`. It is built into a Docker image and never installed directly on a host.

- Operator runs `okto init`; the CLI shells out to `docker pull` + `docker run -d --name lair -v ~/.okto:/data -p 8443:8443 --env-file ~/.okto/lair-env <image>`.
- The Rust code (both CLI and lair) **never imports a Docker SDK**. Every Docker interaction is a shell-out — either from `cli/src/service.rs` for lifecycle, or from the agentic loop's `bash` tool for inside-the-container diagnostics.
- When lair creates a child agent, it spawns `lair --role agent` itself **inside the same container** (via `tokio::process::Command`), recording the child's pid in `/data/lair/agents.json`. There is no second container per agent.
- `lair/src/bootstrap.rs` does the public-IP detection, optional git clone, and runs the operator's `~/.okto/bootstrap.sh` (`/data/bootstrap.sh`) if present — the single startup-customization hook, run once by the container entrypoint (lair, or a standalone remote agent).

Build:

```sh
# CLI — Linux host binaries, published by .github/workflows/cli.yml.
cargo build --release -p okto

# Lair image — multi-arch, built + pushed to ghcr by the lair.yml workflow
# (gh workflow run lair.yml) after each version bump. See "Platform" above.
```

The CLI binaries end up at `target/release/okto`; CI publishes them per-target as Release assets attached to `cli-v<version>`. The lair image is tagged `ghcr.io/georgebradford0/lair:<version>` and `:latest`.

### Tests

The `okto-tests` crate (`tests/`) holds the workspace's black-box e2e suites — run the whole thing with `cargo test -p okto-tests`:

- **lair** (`boot.rs`, `chat.rs`, `tools.rs`) — spawn the real `lair` binary on a temp dir + ephemeral ports and drive it over the Noise tunnel like the mobile client, with an in-process Anthropic-SSE mock LLM (fully offline).
- **CLI** (`cli_*.rs`, sharing `common/okto_cli.rs` + `common/mock_mgmt.rs`) — spawn the real `okto` binary against a temp `HOME` and assert on stdout/stderr/exit code plus on-disk `~/.okto` state. Cover `version`, `completions`, `config`, `env`, `mcp list/remove`, `qr`, `ssh pubkey`, `agents`, and `tasks`; commands that hit lair's loopback management API run against a mock, so no docker or network is needed.

The `cli.yml` release workflow runs the **CLI suites only** (the `cli_*` files, not the lair suites) as a `test` job that `build` depends on, so the CLI tests must pass before any CLI release binary is built or pushed. Locally, `cargo test-cli` (a `.cargo/config.toml` alias) runs that same CLI-only subset.

---

## Architecture overview

okto is an agentic coding assistant. A single `lair` process runs on a host machine, supervises child agent processes via a small `AgentSupervisor`, and exposes itself to a mobile client over an encrypted Noise tunnel. Mobile traffic to child agents is proxied through lair — children never get a public network surface.

### Components

| Directory | Language | Role |
|-----------|----------|------|
| `core/` | Rust | Shared library: agentic loop, Claude API streaming, git ops, config, HTTP/WS plumbing, agent registry, SSH keygen, MCP plumbing, Noise proxy. |
| `lair/` | Rust + Axum | Merged binary `lair` with `--role lair|agent`. `lair/src/lair.rs` is the parent (orchestrates child processes via `lair/src/agent_proc.rs`); `lair/src/agent.rs` is the child. |
| `cli/` | Rust | `okto` CLI for managing lair on the local host (init, reload, agents, logs, mcp, config, env). |
| `mobile/` | React Native (TS) | iOS/Android client: QR scan → native Noise tunnel → single WebSocket to lair → optional proxy URL for chatting with children. |

### Filesystem layout

Everything lives under `~/.okto/` on the host, bind-mounted at `/data` inside the container:

- `~/.okto/config.json` ↔ `/data/config.json` — operator credentials (API keys, model). Read by every role via `okto_core::config_path()`. The Rust code resolves `OKTO_HOME=/data` to find this; the host CLI uses `$HOME/.okto`.
- `~/.okto/lair-env` — KEY=VALUE lines passed to `docker --env-file` on every `start_lair`. Operator-managed via `okto env`. **Stays on the host, not bind-mounted** — only consumed at container-start time.
- `~/.okto/lair-launch.json` — bookkeeping for `okto reload`: ports + last-used image reference.
- `~/.okto/bootstrap.sh` ↔ `/data/bootstrap.sh` — optional operator-managed startup script. If present, the container entrypoint (lair, or a standalone remote agent) runs it once at boot via `bash` before binding its HTTP listener — the single hook for installing extra packages/tools into the shared container. Locally-spawned children skip it (`OKTO_LOCAL_CHILD=1`). Failure aborts boot.
- `~/.okto/lair/` ↔ `/data/lair/` — lair's per-process data dir (`OKTO_DATA_DIR`). Holds `noise_key.bin`, `agents.json`, `mcp.json`, `messages.json`, `tasks.json`, `relay_signing_key.bin`, `known_hosts`. (No more `lair.pid` / `lair.log` — the container lifecycle is tracked by docker; `okto logs` shells out to `docker logs`.)
- `~/.okto/.ssh/` ↔ `/data/.ssh/` — the **container-level** SSH keypair (`id_ed25519`, `id_ed25519.pub`). One key per container; lair generates it on startup and seeds every spawned agent's `~/.ssh/` from it, so the whole container shares one identity. Register the matching pubkey once on external services (Prime Intellect, GitHub, GPU pods, etc.) via `okto ssh pubkey`.
- `~/.okto/agents/<name>/` ↔ `/data/agents/<name>/` — per-agent dirs. Each has `data/` (the agent's `OKTO_DATA_DIR`), `workspace/` (its `WORKSPACE_DIR`), and `.ssh/` (a copy of the container keypair, chowned to the agent's uid), plus an `agent.log` capture written by lair's supervisor. Agents with a git repo can also have a `worktrees/<id>/` dir per git worktree (see "Worktrees" below).

### Worktrees

A repo-bound agent can host **git worktrees** of its own `workspace/.git`, each with its own chat. Worktrees are *not* registry rows — they live inside the one agent process:

- Working dir: `/data/agents/<name>/worktrees/<id>/` (a `git worktree` of `workspace/.git`, on its own branch). Because all worktrees share the agent's single clone and run under the agent's own uid, there's no cross-process permission concern.
- Per-worktree chat state: `/data/agents/<name>/data/worktrees/<id>/{session/messages.json,session/tasks.json}`. The manifest `/data/agents/<name>/data/worktrees.json` is the source of truth (id → {branch, path, created_at}); sessions are rebuilt from it on agent startup, pruning any whose dir vanished.
- The agent process is **multi-session**: `agent.rs`'s `AppState` is one chat session (history, cwd, system, stream fanout, turn gate, cancel), and a process-level `Agent` holds a `HashMap<worktreeId, AppState>` with `""` = the main workspace. Worktree id is a route-safe slug of the branch (`feature/x` → `feature-x`).
- Agent routes: `GET/POST /worktrees`, `DELETE /worktrees/:wt`, and `/worktrees/:wt/{history,stream,interrupt,clear,branches,completions,tasks/:id/cancel}` mirror the top-level chat routes scoped to a worktree. `DELETE` runs `git worktree remove` + `git branch -D` (branch is deleted with the worktree) and drops the chat dir.
- Both clients reach these via lair proxies at `/agents/<name>/worktrees/…` and render worktrees as rows indented under their agent in the sidebar (composite key `<agent>::<wt>` on desktop; paired `activeChild`/`activeWorktree` on mobile).

### Agent registry

`~/.okto/lair/agents.json` is lair's single source of truth for which agents exist. Schema is `okto_core::AgentRecord`:

```
{ name, pid, port, git_url, status, binary_version, created_at, last_seen }
```

`pid` is the recorded OS pid of the last spawned `lair --role agent` process. The poller checks `kill(pid, 0)` every 10s and flips `status` accordingly. On lair startup, the supervisor adopts any rows whose pid is still alive.

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
- `POST   /agents` — `{name?, git_url?, port?, startup_prompt?, mcp?}` — spawn a new child. Omit `mcp` to inherit lair's current `mcp.json` verbatim (the default); pass `[]` for no MCP servers, or a non-empty array (same schema as `mcp.json`) to override.
- `POST   /agents/:name/start` / `stop` — supervisor control.
- `DELETE /agents/:name` — terminate + remove data dir.
- `GET    /agents/:name/logs` — last 1 MB of the child's `agent.log`.
- `GET    /agents/:name/stream` — WebSocket proxy (mobile end).
- `GET    /agents/:name/history`, `POST /agents/:name/interrupt`, `POST /agents/:name/clear`, `GET /agents/:name/branches` — HTTP proxies of the child's same-name endpoints.
- `GET/POST /agents/:name/worktrees`, `DELETE /agents/:name/worktrees/:wt`, and `/agents/:name/worktrees/:wt/{stream,history,interrupt,clear,branches,completions,tasks/:id/cancel}` — proxies of the child's worktree endpoints (see "Worktrees").
- `POST   /tasks/:id/cancel` — cancel a lair background task. Returns `{"id":"…","fired":bool}`.
- `POST   /agents/:name/tasks/:id/cancel` — proxy to the child's same endpoint; used by `okto tasks stop --agent <name> <id>`.

### lair credentials (`~/.okto/config.json`)

Lair reads its API keys and provider settings from `~/.okto/config.json` (mapped to `/data/config.json` in-container). Lair re-reads it on every model call, so rotation is live without restarting the container. Children inherit credentials via env at spawn time.

`GH_TOKEN` and any other operator-supplied env vars live in `~/.okto/lair-env` (operator-managed via `okto env set KEY=VAL`). The file is passed to `docker run --env-file`, so anything in it reaches the lair container's process env — and is then inherited by every child agent process lair spawns (tokio's `Command` default).

### lair runtime env

Baked into the image (see `lair/Dockerfile`):

| Variable | Purpose |
|----------|---------|
| `HOME` / `OKTO_HOME` | `/data` (so config + ssh keys resolve to bind-mounted host paths) |
| `OKTO_DATA_DIR` | `/data/lair` |
| `OKTO_AGENTS_DIR` | `/data/agents` |
| `OKTO_LAIR_BINARY` | `/usr/local/bin/lair` (used to spawn children) |
| `OKTO_SKIP_SHELL_ENV` | Always 1; suppresses the login-shell env sourcing |

Tools baked into the image (besides `lair`): `aws` (AWS CLI v2), `buildah`, `fuse-overlayfs`, `gcc`, `git`, `gh`, `glab`, `jq`, `node`/`npm`, `openssh-client`, `procps` (`ps`/`top`/`free`), `qrencode`, `slirp4netns`, `uidmap`, `unzip`, `uv`/`uvx`. `/etc/subuid` + `/etc/subgid` are populated for every agent uid (10001, 10100..10199) so each can run `buildah` rootless; the image defaults to the `vfs` storage driver (configured via `/etc/containers/storage.conf`).

Set at `docker run` time by `cli/src/service.rs`:

| Variable | Purpose |
|----------|---------|
| `PUBLIC_PORT` | Noise port the QR code advertises (matches the host-side `-p` mapping) |

`NOISE_PORT` inside the container is hardcoded to the `EXPOSE`d 8443; the host-side port mapping is what the operator controls via `--noise-port`.

### agent (child) env (set by lair on spawn)

| Variable | Purpose |
|----------|---------|
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `MODEL` / `OPENAI_API_URL` | Provider credentials |
| `AGENT_PORT` | Loopback HTTP port (30100–30199) |
| `OKTO_DATA_DIR` | `~/.okto/agents/<name>/data` |
| `WORKSPACE_DIR` | `~/.okto/agents/<name>/workspace` |
| `GIT_URL` | Optional repo to clone (HTTPS needs `GH_TOKEN`) |
| `AGENT_PURPOSE` / `STARTUP_PROMPT` | Optional bootstrap knobs |
| `OKTO_LOCAL_CHILD` | Set to `1` for in-container children so they skip the shared `bootstrap.sh` (lair already ran it) |

### MCP inheritance (lair → child)

When a child is created (`create_agent` tool / `POST /agents`), lair writes its current `mcp.json` into `~/.okto/agents/<name>/data/mcp.json` so the child boots with the same MCP servers lair has. Callers can override by passing an explicit `mcp` array in the create request: `[]` for no servers, or a non-empty list matching the `mcp.json` schema for an exact replacement.

Inheritance is a snapshot at create time — subsequent edits to lair's `mcp.json` do not propagate. Per-agent edits via `okto mcp add --agent <name>` survive restarts because `start_agent_by_name` passes `mcp: None` to the supervisor, which leaves the child's existing `mcp.json` untouched.

### CI/CD workflows (all manual dispatch)

| Workflow | What it does |
|----------|--------------|
| `cli.yml` | Runs the CLI e2e suites (the `cli_*` tests, gate), then builds the `okto` CLI per-target and uploads as Release assets. The `build` job `needs:` the `test` job, so a failing test blocks the release. |
| `relay.yml` | Builds `okto-relay` per-Linux-arch and uploads as assets on `relay-v<version>` (read from `relay/Cargo.toml`). |
| `lair.yml` | Builds the multi-arch lair Docker image (native amd64 + arm64 runners) and pushes `ghcr.io/georgebradford0/lair:<version>` + `:latest` (version from `lair/Cargo.toml`). |
| `desktop.yml` | Builds the Tauri desktop app as a Universal macOS DMG, signs + notarizes, attaches to the `desktop-v<version>` release. |
| `android.yml` | Builds AAB via fastlane, uploads to Google Play. |
| `ios.yml` | Builds on macOS runner, optionally uploads to TestFlight. |

The lair image **is** built by CI now (`lair.yml`); a local `docker buildx` build (see the "Platform" section) remains a fallback.
