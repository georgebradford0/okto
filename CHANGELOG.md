# Changelog

## [Unreleased]

### Added (this revision)

- **Remote agents restored** as native processes on a Linux VM. Three lair tools come back: `mint_bootstrap_userdata`, `register_remote_agent`, `forget_agent`. The cloud-init userdata now downloads the matching `octo-lair` release artefact and installs a systemd unit running `--role agent` — no Docker on the remote side. Lair finishes the bootstrap over SSH (config.json drop, optional git clone, `systemctl restart octo-agent`). Per-VM layout: `/var/lib/octo/{data,workspace}/`, env file at `/etc/octo/agent.env`, unit at `/etc/systemd/system/octo-agent.service`.
- **Encrypted lair → remote-agent transport.** New `octo_core::open_noise_tunnel` opens an outbound Noise tunnel as the **initiator**, verifying the responder's static pubkey against the registry. Lair's WebSocket/HTTP proxy uses this for any agent with `host = Some(_)` — mobile ↔ lair ↔ remote-agent traffic is end-to-end Noise-encrypted. Added `from_base32` + `noise_handshake_initiator` to `core/src/noise.rs`.
- **Optional Noise responder in agent role.** Setting `AGENT_NOISE_PORT` (only done by the cloud-init userdata) makes the agent run its own `run_noise_proxy` on a public port and publish `<OCTO_DATA_DIR>/agent-info.json` for the SSH-pull. Local agents leave it unset and stay loopback-only.

### Changed (this revision)

- **`AgentRecord` schema** — `git_url` removed (it was never a precondition; the cloned repo lives in the workspace dir and `bootstrap::ensure_workspace` detects it via `.git`). `host`, `pubkey`, `instance_id`, `provider`, `metadata` restored for remote agents. `is_remote()` predicate restored (`host.is_some()`). The poller skips pid liveness for remote rows; `terminate_agent` / `start_agent_by_name` refuse remote rows with helpful guidance.
- **Wire `agents` event** — drops `git_url`, adds `kind: "local" | "remote"`. Mobile sidebar labels remote agents accordingly.
- **`octo agents list`** column set is now `NAME / KIND / STATUS / PORT / PID / HOST` (was `NAME / STATUS / PORT / PID / GIT URL`).

### Changed

- **BREAKING — Docker removed.** Lair and every child agent now run as plain OS processes on a Linux host. The `bollard` dep is gone from both `lair` and `cli`; `lair/Dockerfile`, `docker-compose*.yml`, and `.dockerignore` are deleted. `octo init` spawns `octo-lair --role lair` directly (pidfile at `~/.octo/lair/lair.pid`); children spawn as `octo-lair --role agent` via `tokio::process::Command` in a new `lair/src/agent_proc.rs` supervisor. Per-agent state lives in `~/.octo/agents/<name>/{data,workspace}/` (replaces named volumes). No migration is provided — this project has no users yet.
- **BREAKING — Mobile traffic proxied through lair.** Mobile only ever holds one Noise tunnel (to lair). To chat with a child, mobile opens a WebSocket to `ws://lair/agents/<name>/stream`; lair proxies frames to the child's loopback HTTP port via `tokio_tungstenite`. Children no longer have a public network surface, a per-agent Noise keypair, or a QR code. The wire `ContainerInfo` payload's `host`/`port`/`pubkey` fields are gone.
- **BREAKING — Wire schema renamed.** `containers` event → `agents`; `start_container` frame → `start_agent`. `core::ChatEvent::Containers` → `Agents`, `core::ChatEvent::StartContainer` → `StartAgent`, with a new `TerminateAgent` variant. `mobile/src/wire.ts` mirrors these.
- **BREAKING — Remote-agent feature removed.** `mint_bootstrap_userdata`, `register_remote_agent`, and `forget_agent` lair tools deleted; SSH-driven cloud-init flow and `lair/src/ssh.rs` removed. Without Docker on remote VMs the cloud-init shape would have changed anyway; defer this until a sandboxing story exists.
- **BREAKING — Linux only.** macOS and Windows builds dropped from `cli.yml`. The CLI uses `kill(pid, 0)` for lair liveness; `octo destroy` SIGTERMs the pid directly.
- **`AgentRecord` schema simplified.** Renamed `container_id` → `pid: Option<u32>`, `image_version` → `binary_version`. Dropped `host`, `pubkey`, `instance_id`, `provider`, `metadata`. `status_from_docker` helper becomes `status_from_alive(bool)`. `Registry::update_container_id` → `update_pid`.
- **`octo-lair` binary distribution.** `scripts/get-cli.sh` now downloads both `octo` and `octo-lair` per-target; the CLI locates the lair binary via `$OCTO_LAIR_BINARY`, `which octo-lair`, sibling `octo-lair`, or `~/.octo/bin/octo-lair`.
- **`config.json` now lives at `~/.octo/config.json` regardless of `OCTO_DATA_DIR`** (via new `octo_core::config_dir()`). Lets lair, every agent, and the CLI share a single config file without bind-mount gymnastics.

### Removed

- `lair/src/docker.rs`, `cli/src/dockerd.rs` (bollard wrappers).
- `lair/src/ssh.rs` (remote-agent SSH client).
- `docker-compose.yml`, `docker-compose.prod.yml`, `lair/Dockerfile`, `.dockerignore`, `down.sh`, `deploy/`.

### Historical (pre-Docker-removal) notes preserved below

- **Kubernetes removed.** lair ran as a single Docker container on a host machine and orchestrated children via the local Docker daemon. The `octo-k8s-ops` crate, `k8s/` manifests, and `docs/kubernetes-migration.md` were deleted. `octo init` no longer installs k3s; it `docker run`s lair instead, bind-mounting `~/.octo/lair/` to `/data` and the host Docker socket.
- **New `~/.octo/lair/agents.json` registry** owned by lair, replacing the prior Deployment-label model. CLI reads it for `octo agents list`; mutations (`start` / `stop` / `delete`) go through Docker and are reconciled by lair's 10 s poller.
- **Removed `message_lair` / `message_child` tools** and the `LAIR_URL` plumbing that supported them. Lair is now a pure orchestrator: it creates / inspects / terminates children; user-to-child conversations happen on the child's own mobile chat. Drops a chunk of dead code (lair's HTTP client, agent's reqwest pipeline) and one env var from the agent contract.

### Added

- **Remote-agent provisioning** via three new lair tools (`mint_bootstrap_userdata`, `register_remote_agent`, `forget_agent`). The LLM composes them with any cloud-provisioning MCP (AWS, Hetzner, GCP, …) so adding a new provider requires zero lair changes. Lair's pre-generated SSH key (from `octo init`) is the authentication channel.
  - **The userdata carries no credentials.** It only trusts lair's SSH pubkey, installs Docker + git, and starts the agent in a minimal mode. API keys, the `git_url` clone, and the post-boot restart all run over the SSH connection lair opens during `register_remote_agent` — so the operator's `ANTHROPIC_API_KEY`, `GH_TOKEN`, and the repo URL never flow through the cloud provider or the provisioning MCP.
  - `register_remote_agent` orchestrates the full bootstrap: waits for the agent's `agent-info.json`, drops `config.json` with API keys, runs `git clone` (with token-rewrite + `credential.helper`) if `git_url` was given, then `docker restart`s the agent so it picks up the workspace and credentials cleanly.
  - `bootstrap::ensure_workspace` learned to detect a pre-existing `.git` dir when no `GIT_URL` is set — supports the lair-driven clone path so the agent ends up with the repo-bound system prompt.
  - **Retry + resume**: each one-shot SSH op (`ssh::write_file`, `ssh::run_script`) retries internally up to 4 times with exponential backoff (2s → 16s), absorbing sshd-during-cloud-init flakes silently. `register_remote_agent` writes a `Pending` registry row as soon as the agent's identity is known, surfacing the in-progress agent to mobile and `list_agents` immediately; if a later SSH phase hard-fails, the row stays Pending so a second `register_remote_agent` call with the same `name + host` resumes from the top (every SSH phase is idempotent). A new `Registry::set(record)` upsert replaces the prior name-conflict error for this resumable path.
  - `AgentRecord` gains `provider: Option<String>` and `metadata: serde_json::Value` for provider-side bookkeeping. The lair Docker image now ships `openssh-client`.
- **`octo init` now generates an Ed25519 SSH keypair** at `~/.octo/lair/ssh_id_ed25519{,.pub}`. Reserved for ops backchannels (e.g. tailing logs on a remote-provisioned VM); idempotent — existing keys are left untouched.
- **`/child-version` endpoint removed.** Image versions are recorded in the registry at agent-create time; no child round-trip needed.

## [0.1.2] - 2026-04-06

### Fixed
- **Session bubble preserved on reconnect** — server now assigns a UUID to each agentic session via `session_start`; mobile client uses it to find and reuse the existing session bubble when the server replays `session_start` after reconnect, preventing a duplicate stale bubble appearing alongside the live one

## [0.1.1] - 2026-04-06

### Changed
- **Session lifecycle enforced for all tool calls** — system prompt and tool descriptions now require `session_start` as the very first tool call and `session_end` as the very last, regardless of how many tools are used; previously "non-trivial work" wording left a loophole for single/quick calls

### Fixed
- **MCP child process detach** — replaced non-existent `child.forget()` with `std::mem::forget(child)` to correctly detach spawned MCP server processes from the tokio runtime

## [0.1.0] - 2026-04-06

### Added
- **Connection status dot in chat header** — 8×8 colored circle to the left of the "octo" title indicates server connection state (green = ready, yellow = connecting/streaming, red = error)

### Fixed
- **Noise tunnel re-establishment on app foreground** — AppState listener in `AppInner` now calls `NoiseConnection.disconnect()` + `NoiseConnection.connect()` when the app resumes from background, fixing silent WebSocket reconnect failures caused by iOS suspending the native Noise TCP proxy

### Changed
- **Full server + mobile rewrite** — simplified the entire system end-to-end:
  - **Server (`server/src/main.rs`)**: single session, new wire protocol (`history` / `token` / `tool` / `question` / `done` / `error`), live event buffer with generation counter for safe reconnect replay, `deliver_current` flag prevents duplicate delivery when history already contains a completed response. Removed: worker sessions, session IDs in URLs, event log (.jsonl), seq tracking, `/workers` route, UUID usage, per-session HashMaps
  - **Mobile (`mobile/App.tsx`)**: rewritten from ~1,600 lines to ~680; simplified types (`Message`, `ConnStatus`, `ServerFrame`); three clear screens (connecting spinner, connection picker, chat); token accumulation streams assistant replies inline; AsyncStorage cache per connection; `sendMessageRef` pattern retained to avoid stale closures
