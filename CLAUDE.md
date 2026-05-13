# octo — project notes for Claude

`@` is used as a path prefix to reference files in the repository (e.g. `@src/main.rs`).

## Git workflow

Do **not** create git worktrees unless explicitly asked to. Commit and push directly on the current branch.

Do **not** commit debug/diagnostic logging (`println!`, `console.log`, etc. added purely for investigation). Suggest the user add logs locally instead.

## Docker images

One image, one binary, two roles. Both the parent (lair) and child (agent) containers run the same `octo-lair` binary. The image's ENTRYPOINT runs the lair role; lair creates child containers with `command: ["/usr/local/bin/octo-lair", "--role", "agent"]` to flip the role.

| Image | Used by |
|-------|---------|
| `ghcr.io/georgebradford0/lair` | lair (parent) and every child container |

There are no shell entrypoint scripts — `lair/src/bootstrap.rs` does the public-IP detection, optional git clone, STARTUP_SCRIPT execution, and post-listen QR rendering directly in Rust.

Each child gets two Docker named volumes (`agent-<name>-data`, `agent-<name>-workspace`) and its Noise port (9000 inside the container) published on a host port from the 30100–30199 range. A child container is **not** required to clone a git repo: if `GIT_URL` is set the workspace is populated from that repo (with `GH_TOKEN` for HTTPS); if unset, the workspace is just `mkdir -p /workspace` and the agent runs there as a general-purpose agent. Set `AGENT_PURPOSE` (env var) to give a no-repo agent a specific mission in its system prompt.

Build and push from the **repo root** (replace `X.Y.Z` with the new version). Always use `buildx` with `--platform` so both `linux/amd64` and `linux/arm64` are included in the manifest:

```sh
docker buildx build \
  --builder multiplatform \
  --platform linux/amd64,linux/arm64 \
  --push \
  -f lair/Dockerfile \
  -t ghcr.io/georgebradford0/lair:X.Y.Z \
  -t ghcr.io/georgebradford0/lair:latest \
  .
```

---

## Architecture overview

Octo is an agentic coding assistant: a single `lair` Docker container runs on a host machine, manages child agent containers via the local Docker daemon, and exposes itself to a mobile client over an encrypted tunnel.

### Components

| Directory | Language | Role |
|-----------|----------|------|
| `core/` | Rust | Shared library: agentic loop, Claude API streaming, git/worktree ops, config, HTTP/WS plumbing (`core::app`), agent **registry** types (`AgentRecord`, `Registry`), SSH keygen, MCP plumbing |
| `lair/` | Rust + Axum | Merged binary `octo-lair` with `--role lair\|agent`. `lair/src/lair.rs` is the parent (orchestrates child containers via `lair/src/docker.rs`); `lair/src/agent.rs` is the child (general-purpose agent, optionally repo-scoped). Both are reachable from `lair/src/main.rs`'s argparse dispatch. |
| `cli/` | Rust | `octo` CLI for managing the local Docker host (init, reload, agents, logs, mcp, config). |
| `mobile/` | React Native (TS) | iOS/Android client: QR scan → native Noise tunnel → WebSocket UI |

### Agent registry

The list of agents lair owns lives in `<lair_data_dir>/agents.json` (`/data/agents.json` inside the container, bind-mounted to `~/.octo/lair/agents.json` on the host by default). Lair is the sole writer; the CLI reads it for `octo agents list`. Schema is `octo_core::AgentRecord`: `name`, `container_id`, `host`, `port`, `pubkey`, `git_url`, `status`, `image_version`, `created_at`, `last_seen`, `instance_id`, `provider`, `metadata`. `AgentRecord::is_remote()` returns true when the record represents a VM-backed agent (no `container_id`).

Lair's poller runs every 10 s, calls `docker list_containers` for things labelled `octo.managed=1`, and reconciles each *local* row's status. Containers that have disappeared from Docker are dropped from the registry on the next poll (so `octo agents delete <name>` from the CLI cleans up cleanly). Remote rows are skipped — they're surfaced to mobile as-is and stay in the registry until `forget_agent` removes them.

### Remote agents

Lair can also register agents that live on a VM provisioned via a third-party cloud-provisioning MCP (AWS, Hetzner, GCP, etc.). It's a three-step LLM-driven flow, with the operator's API keys *never* flowing through the cloud provider or the provisioning MCP — lair owns the SSH connection and finishes the bootstrap directly:

1. `mint_bootstrap_userdata(name, agent_purpose?, startup_script?, public_port?)` returns a cloud-init bash script. The userdata is **credentials-free** — it only trusts lair's SSH public key, installs Docker + git, prepares bind-mount dirs (`/var/lib/octo/agent-data` ↔ `/data`, `/var/lib/octo/agent-workspace` ↔ `/workspace`), and starts the agent container with non-secret env (PUBLIC_PORT, NOISE_PORT, OCTO_DATA_DIR, …). The agent boots without API keys, writes its `agent-info.json`, and waits.
2. The LLM hands the userdata to whichever provisioning MCP is configured. The MCP returns a public IP and instance id.
3. `register_remote_agent(name, host, git_url?, provider?, instance_id?, metadata?)` runs everything secret-bearing over SSH using lair's private key (`/data/ssh_id_ed25519`):
   a. polls `/var/lib/octo/agent-data/agent-info.json` until the agent publishes its identity (cloud-init delays absorbed),
   b. writes `/var/lib/octo/agent-data/config.json` with API keys harvested from lair's own env (Anthropic / OpenAI keys + model),
   c. if `git_url` is given, clones it into the workspace using lair's `GH_TOKEN` (URL-spliced, plus a `credential.helper` for `git push`),
   d. `docker restart`s the agent so it re-runs `ensure_workspace` (detects the freshly-cloned repo → repo-bound system prompt) and picks up `config.json`,
   e. inserts an `AgentRecord` with `host=Some(<public_ip>)`, `instance_id=Some(...)`, and any provider `metadata`.

Mobile reads the new row from its usual `containers` event and opens a Noise tunnel directly to `<public_ip>:<port>` — no `octo init`-style QR scan needed.

Termination is the reverse: lair has no embedded cloud SDK, so the LLM calls the provisioning MCP's terminate-instance method with `instance_id` from `list_agents`, then `forget_agent(name)` clears the registry row.

### Transport

All client↔server communication is encrypted with **Noise_XX_25519_ChaChaPoly_SHA256**:

1. Client scans QR code → `2:<host>:<port>:<base32-pubkey>`
2. TCP connection → 3-message Noise XX handshake. The QR-pinned server static key is verified during the handshake; the client's static key is captured from snow's `get_remote_static()` and logged at handshake completion (per-client allowlisting is plumbed but not yet enforced).
3. Frame format: 2-byte big-endian length prefix + ciphertext. Frames over `MAX_FRAME_SIZE` (16 KiB) are rejected; body reads timeout after 30 s; the whole handshake must complete within 10 s. Per source IP, no more than 32 concurrent Noise sessions are accepted.
4. Post-handshake: a single, persistent WebSocket runs over the encrypted frames. One WS per server (lair or child) for the entire chat session — opened on chat-screen mount, closed on unmount. `core/src/lib.rs::ChatEvent` is the wire schema; tagged JSON via serde `tag = "type"`.

Wire frames (server → client unless noted):

| `type`            | Direction | Payload                                                   |
|-------------------|-----------|-----------------------------------------------------------|
| `ready`           | s → c     | `session_id: string`, `resumed: bool`. Sent once on open. |
| `text`            | s → c     | `text: string`. Streamed live: one delta per Anthropic `content_block_delta` (text_delta) or OpenAI `choices[0].delta.content`. Mobile appends to a single message id. |
| `tool_use`        | s → c     | `tool: string`, `input: any`. Emitted once per tool call after its input JSON is fully assembled. |
| `tool_output`     | s → c     | `line: string`. One per stdout/stderr line during tool execution. |
| `tool_result`     | s → c     | `tool_use_id: string`, `output: any`. Final tool result. |
| `done`            | s → c     | `cost_usd: number`. Turn finished cleanly. |
| `interrupted`     | s → c     | `cost_usd: number`. Turn cancelled by client. |
| `interrupt_ack`   | s → c     | (no fields) Ack of an `interrupt` frame. |
| `error`           | s → c     | `message: string`. Turn failed; WS stays open. |
| `system`          | s → c     | `text: string`. Server status string for the UI. |
| `containers`      | s → c     | `containers: ContainerInfo[]`. **Lair only.** Pushed on poller change. |
| `ping`            | s → c     | `id: number`. Liveness probe every `KEEPALIVE_INTERVAL` (15 s). |
| `user_message`    | c → s     | `text: string`. Start a new agentic turn. |
| `interrupt`       | c → s     | (no fields) Cancel the in-flight turn. |
| `pong`            | c → s     | `id: number`. Reply to `ping`. After `KEEPALIVE_MAX_MISSED` (2) unacked pings the server evicts the WS. |
| `start_container` | c → s     | `id: string`. **Lair only.** Start the named (stopped) agent container. |

Mobile auto-reconnects with exponential backoff (1 s → 30 s, capped) on unintentional close; the counter resets on the next `ready`.

Lair listens on port 9000 (`NOISE_PORT`) inside the container; the host publishes it on whatever port `octo init` set (default 8443). Lair's Curve25519 keypair and registry are persisted in `/data` (bind-mounted from `~/.octo/lair/` on the host).

### lair (parent container)

`lair` is the parent orchestration node. The mobile client connects to it first via the QR-scanned Noise tunnel. It:

- Polls Docker (every 10 s) for containers labelled `octo.managed=1` and reconciles them against `/data/agents.json`
- Pushes the current container list as a `containers` event over `/stream` on every state-change (mobile subscribes; no HTTP polling)
- Accepts `start_container` frames from the client over `/stream`, which `docker start` the named agent and trigger an immediate re-poll
- Runs its own agentic loop (via `core`) so the user can ask it to create / inspect / terminate child containers from chat

Image: `ghcr.io/georgebradford0/lair`

#### lair credentials (read from `/data/config.json`)

Lair reads its API keys and provider settings from `/data/config.json` —
the operator's `~/.octo/config.json` bind-mounted read-only into the
container. `docker inspect lair` therefore does NOT expose them. The CLI
writes this file (`octo init` / `octo config set`); lair re-reads it on
every model call, so credential rotation is live, no restart needed.

| Field in `config.json` | Required | Purpose |
|----------|----------|---------|
| `anthropic_api_key` | yes | Claude API access (also forwarded to children) |
| `model` / `api_url` / `openai_api_key` | no | OpenAI-compatible provider for both lair and children |

`gh_token` used to live here too; it doesn't any more. `GH_TOKEN` is now a
plain env var on lair (operator-supplied via `octo init --env GH_TOKEN=…`
or, in dev, forwarded from the host shell by `start_dev.sh`), and lair
reads it from `std::env::var("GH_TOKEN")` when it forwards to children,
clones for remote agents, or shells out to `gh`/`git`. The trade-off is
explicit: `GH_TOKEN` will appear in `docker inspect lair`, unlike the
config-mounted secrets.

#### lair runtime environment variables (non-secret)

| Variable | Required | Purpose |
|----------|----------|---------|
| `PUBLIC_HOST` | no | Advertised host in QR (auto-detected via `api.ipify.org` if unset) |
| `PUBLIC_PORT` | no | Externally-reachable port (defaults to `NOISE_PORT`) |
| `NOISE_PORT` | no | Listening port inside the container (default: 9000) |
| `NOISE_KEY_FILE` | no | Path to the Curve25519 private-key file (default: `/data/noise_key.bin`, generated on first run if absent) |
| `OCTO_AGENT_IMAGE` | no | Image tag used when lair creates child containers (default: `ghcr.io/georgebradford0/lair:latest`) |
| `OCTO_DATA_DIR` | no | Lair's data dir (default: `/data` inside the container; resolves to `~/.octo` on bare-host invocations of the CLI) |

### agent (child container) environment variables

| Variable | Required | Purpose |
|----------|----------|---------|
| `ANTHROPIC_API_KEY` | yes | Claude API access |
| `GIT_URL` | no | Repo to clone into the workspace. Without it the agent runs in `/workspace` as a general-purpose agent (no git involvement). |
| `GH_TOKEN` | no | GitHub token for HTTPS clones / PR creation. Required when `GIT_URL` is HTTPS. |
| `AGENT_PURPOSE` | no | One-line mission statement baked into the system prompt when no `GIT_URL` is set |
| `WORKSPACE_DIR` | no | Workspace path (default: `/workspace`) |
| `STARTUP_SCRIPT` | no | Bash snippet run at boot after the workspace is populated |
| `STARTUP_PROMPT` | no | First user message handed to the agentic loop on boot |
| `PUBLIC_HOST` | no | Advertised host in QR (auto-detected if unset) |
| `PUBLIC_PORT` | no | Externally-reachable port (defaults to `NOISE_PORT`) |
| `NOISE_PORT` | no | Listening port (default: 9000) |
| `GIT_USER_NAME` / `GIT_USER_EMAIL` | no | Commit author identity |

### CI/CD workflows (all manual dispatch)

| Workflow | What it does |
|----------|-------------|
| `android.yml` | Builds AAB via fastlane, uploads to Google Play (closed/production track) |
| `ios.yml` | Builds on macOS runner, optionally uploads to TestFlight |
