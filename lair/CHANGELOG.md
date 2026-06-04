# Changelog

## [Unreleased]

### Fixed
- **`create_agent` is now transactional under interrupt.** Interrupting the turn
  while a `create_agent` tool call is in flight previously left a half-created
  agent behind: the child process is spawned with `kill_on_drop(false)` and is
  already in the registry by the time lair waits for its `/health`, so the
  cancel-by-drop only abandoned the wait — the orphaned process + `Pending`
  registry row + on-disk dirs survived and the poller would even promote it to
  `running`, all while the model was told "interrupted". A drop guard now rolls
  the half-created agent back: it SIGTERM→(grace)→SIGKILLs and reaps the child,
  removes its `data/`/`workspace/`/`.ssh` dirs, drops the registry row, and frees
  the capability-token slot — leaving no trace. The guard is disarmed on a
  successful create and on the deliberate "spawned but not yet reachable, leave
  it for the operator/poller" outcome, so only a true interrupt triggers rollback.

## [0.22.1] - 2026-06-04

### Fixed
- **Interrupt handling no longer races the turn boundary.** The per-turn cancel
  token now lives inside the `TurnGate`, so installing the next turn's token,
  clearing `interrupt_requested`, and the interrupt itself all happen under one
  lock. Previously an interrupt arriving in the gap between a turn finishing and
  the next (auto- or queued-message) turn starting could cancel the
  already-finished turn's token and be swallowed, letting the chained turn run
  uncancelled. Three related fixes: an interrupt received while no turn is
  streaming is now a no-op (it can no longer leave `interrupt_requested` set to
  suppress a later turn's auto-chain); a turn is classified interrupted-vs-done
  from the agentic loop's own outcome rather than a post-hoc token re-sample
  (so a stop pressed just as a turn finishes can't mislabel a completed turn);
  and `STARTUP_PROMPT` turns are now interruptible. No wire-protocol change.
- **Interrupting a `bash` tool now kills the whole process group**, not just the
  `bash -c` leader. The tool runs as its own group leader and an interrupt
  signals the group, so anything the command spawned (a build, a dev server,
  `docker run`, …) is killed too instead of orphaning to PID 1 and continuing to
  mutate the container. The interrupted tool result now also carries the output
  captured before the kill plus an explicit "interrupted before completion"
  marker, so the model can tell how far a non-idempotent command got on resume.
  (Previously a generic cancel-by-drop guard could pre-empt bash's own cleanup,
  bypassing the kill and discarding partial output; bash is now awaited directly
  while other tools keep the universal cancel-by-drop guard.)

## [0.22.0] - 2026-06-03

### Added
- **Inter-agent messaging, routed through lair.** Lair's model gets a
  `send_message_to_agent(agent, message)` tool and every agent's main chat gets
  a `send_message_to_lair(message)` tool. A sent message is *injected* into the
  recipient's conversation (a persisted-only `peer_message`) and wakes its model
  to act on it at the next turn boundary — the same mechanism background-task
  completions use. Delivery is asynchronous and fire-and-forget: any reply
  arrives later as its own `[message from …]` injection, not as the tool result.
  A new `peer_message` `ChatEvent` is fanned out live (additive — no
  `WIRE_PROTOCOL` bump), and peer messages now appear in `/history`. Worktree
  sessions neither send nor receive peer messages — main chats only.

### Fixed
- **Every agent now receives a capability token at spawn** (previously only
  agent-spawned children did). This (a) gives every agent the credential the new
  messaging channel needs, and (b) lets **top-level/operator-spawned agents
  spawn children** within `agent_spawn_max_depth` — previously the
  agent-spawn-agent feature could never bootstrap, because the only token-mint
  path required an existing token. Spawn remains bounded purely by the
  server-side depth/descendant caps.

## [0.21.7] - 2026-06-03

### Changed
- **Agents are keyed by a route-safe `slug`, decoupled from the display
  `name`.** An agent's free-form `name` (which may contain spaces) is no longer
  used to build its on-disk dir or the wire `id` / proxy route. Each
  `AgentRecord` now carries a `slug` (derived from the name via
  `okto_core::sanitize_agent_name`) that is the registry key, the
  `/data/agents/<slug>/` path, the wire `id`, the route param, and the `parent`
  back-reference. The `agents` event now sends `id` = slug and `name` = display
  label. The `terminate_agent` / `forget_agent` tools accept either the slug or
  an unambiguous display name. **Bumps `WIRE_PROTOCOL` 1 → 2** (the meaning of
  the `agents` event's `id` changed). See `PROTOCOL.md`.

## [0.21.6] - 2026-06-02

### Added
- **Wire protocol version advertised.** lair now reports `wire_protocol`
  (`okto_core::WIRE_PROTOCOL`) and `lair_version` on `GET /info` and in every
  `ready` frame, so clients can detect when they're talking to an
  incompatible-vintage host. lair stays backward-compatible across all client
  versions and never rejects a client on version. See `PROTOCOL.md`. Covered by
  e2e (`tests/tests/boot.rs`).

## [0.21.5] - 2026-06-02

### Added
- **Friendly tool labels in `/history`.** Tool rows returned by `GET /history`
  (and the agent-side equivalent) now carry an additive `display` field with the
  same natural-language phrase the live `/stream` sends (e.g. "Editing file
  (src/x.rs)"). `text` is unchanged (raw `name(arg)`) so existing clients are
  unaffected; the mobile client prefers `display` so a finished tool no longer
  reverts to the bare tool name after a history reload. Covered by
  `tests/tests/tools.rs`.

### Changed
- **Auto-named agents.** When a new agent is created without an explicit `name`
  (via the `create_agent` tool or `POST /agents`), lair now asks the model for a
  short, memorable name derived from the spawn context (git URL + startup
  prompt) — a single tool-less model call. Passing `name` still uses it verbatim
  and skips the call. If model naming fails for any reason, lair falls back to
  the previous deterministic default (`lair-<repo-slug>`, or `lair-workload`).
  Generated names that collide with an existing agent get a numeric suffix
  rather than failing the spawn. Covered by e2e tests
  (`tests/tests/agent_create.rs`).

## [0.21.4] - 2026-06-01

### Added
- **Agent worktree tools — `create_worktree` / `remove_worktree`.** A child
  agent with a git repo attached can now manage its own git worktrees from the
  agentic loop: `create_worktree` adds a worktree on a fresh branch (isolated
  checkout + its own chat tab) and returns its path; `remove_worktree` tears one
  down and deletes its branch. The tools are gated on a repo being attached (the
  workspace is a clone) and are agent-only — lair never offers them. They drive
  the agent's existing `/worktrees` HTTP handlers over loopback, so client- and
  model-initiated worktree changes share one code path. Covered by e2e tests
  (`tests/tests/worktree_tools.rs`).

## [0.21.3] - 2026-06-01

### Removed
- **AWS CLI v2 is no longer baked into the lair image.** It added ~150 MB and
  was unused by default. Operators who need it can install it from their
  `~/.okto/bootstrap.sh` startup hook (the `unzip` the official installer
  requires is still in the image).

## [0.21.2] - 2026-05-31

### Added
- **Cost reporting for OpenAI-compatible backends.** Optional
  `cost_input1M` / `cost_output1M` fields in `~/.okto/config.json` set the
  USD price per 1M input/output tokens. When both are present, per-turn cost
  for the OpenAI-compatible path is computed from these rates (re-read each
  turn, so edits take effect without a restart); when either is absent it
  stays 0.0, the prior behavior. Anthropic continues to use its built-in
  pricing and ignores these fields.

## [0.21.1] - 2026-05-31

No functional change from 0.21.0. Republished to verify the release pipeline
and to keep the lair image version cadenced with the CLI release that ships
the `--ready-timeout` fix (okto 0.7.1).

## [0.21.0] - 2026-05-31

### Added
- **Push notifications can now be disabled end-to-end** via `OKTO_RELAY_URL=`
  (empty) in the container env, typically set by `okto init --disable-push`.
  When disabled, both lair and child agents omit the `send_notification` and
  `ask_question` tools from the model's tool list (instead of letting the
  model call them and get a "push disabled" stub back), `/info` advertises an
  empty `relay_url`, and the mobile client's relay registration becomes a
  no-op.
- **End-to-end test suite for the lair binary** (`tests/` crate). Spawns the real `lair --role lair` process on a temp data dir with ephemeral ports, drives it over the Noise tunnel exactly like the mobile client, and asserts streamed `ChatEvent` frames + on-disk state — with an in-process Anthropic-SSE mock LLM, so the tests are fully offline (no API spend, no Docker). Covers boot/transport, a full chat turn, history persistence, `/clear`, mid-turn interrupt, and real `bash`-tool execution. Run with `cargo test -p okto-tests`.

### Changed
- **`OKTO_RELAY_URL=""` (explicit empty) now disables push** instead of
  silently falling back to the default relay. Unset still falls back; only the
  explicit-empty form turns push off. This is what `okto init --disable-push`
  writes into `lair-env`.
- **`OKTO_HTTP_PORT` env override** for lair's loopback HTTP port (still defaults to 8000). Lets multiple lair processes run side by side; used by the e2e suite for port isolation.
- **`ANTHROPIC_API_URL` env override** (in `okto-core`) for the Anthropic `/v1/messages` endpoint (defaults to the real API). Lets the e2e suite point the production request path at a mock server.
- **Tool results now stream to clients.** The live chat loop (`okto_core::send_message`) now emits a `tool_result` frame after each tool finishes, matching the wire schema desktop/mobile already implement. Previously only the (unused) startup loop did this, so the desktop UI had to infer tool completion from `done`/`interrupted`; per-tool "running" state now clears correctly.

### Removed
- **Consolidated the duplicated agentic-loop code in `okto-core`.** There were three copies of the Anthropic request-building/streaming logic (`call_turn`, `send_message`, and the dead `run_agentic_loop`/`run_startup_prompt` chain) — the duplication had already caused one bug (a missed `ANTHROPIC_API_URL` redirect). `send_message` now builds each turn through the single shared `call_turn`, and the unused `run_agentic_loop`, `run_startup_prompt`, and `Session` struct were deleted. No behavior change to the live path beyond the `tool_result` streaming above.

## [0.20.0] - 2026-05-30

### Added
- **`ask_question` tool.** Both the lair and agent agentic loops now expose an `ask_question` tool the model calls when it is blocked needing operator input. It sends a push notification to the operator's phone (distinct `question` relay category) and then stops and waits for the operator's reply as their next message. Lair signs and POSTs to the relay itself; child agents forward to lair's `/internal/notify`.

## [0.19.0] - 2026-05-29

### Added

- **`procps` baked into the image.** Provides `ps`, `top`, `free`, `uptime`, etc. so agents and lair can inspect process/memory state from the bash tool. (Debian Trixie's `procps` package.)

## [0.18.0] - 2026-05-29

### Added

- **`POST /tasks/:id/cancel` HTTP endpoint** (and `/agents/:name/tasks/:id/cancel` proxy) for stopping background tasks from outside the WS. Twin of the existing `cancel_task` WS frame — same `core_cancel_task` plumbing, returns `{"id":"…","fired":bool}`. Powers the new `okto tasks stop` CLI command.

- **AWS CLI v2 (`aws`) baked into the image.** Installed from AWS's official self-contained zip (no Debian package exists). Multi-arch — picks `x86_64`/`aarch64` based on `dpkg --print-architecture` at build time. Adds ~150 MB to the image. Auth via the usual env vars (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION`) set via `okto env set`; or `aws configure` per session.

- **`glab` CLI (GitLab) baked into the image** (Debian Trixie's 1.53.0 package from main). Mirrors `gh`'s role for GitLab — agents can use it from their bash tool. Auth via `GITLAB_TOKEN` env or `glab auth login` per session.

- **Buildah for daemonless container image builds.** The lair image ships [Buildah](https://buildah.io) plus `uidmap`, `slirp4netns`, and `fuse-overlayfs`. Lair (root) builds rootful; each child agent uid (10001 + 10100..10199) has a 65536-wide subordinate-uid range in `/etc/subuid` + `/etc/subgid` so it can build rootless. The image defaults to the `vfs` storage driver (configured via `/etc/containers/storage.conf`) — slow and disk-heavy, but works inside any Docker container without `/dev/fuse`, special caps, or extra `docker run` flags. `/etc/containers/registries.conf` pre-configures docker.io, ghcr.io, quay.io as unqualified-search registries; `/etc/containers/policy.json` defaults to `insecureAcceptAnything` so any image source is accepted. Per-agent build storage lives under `$HOME/.local/share/containers/storage`. System-prompt note updated for both lair and agents — typical flow is `buildah login … && buildah bud -t … && buildah push …`.

### Changed

- **Startup customization is now a single `~/.okto/bootstrap.sh` file instead of the `STARTUP_SCRIPT` env var.** On startup the container's entrypoint process checks `$OKTO_HOME/bootstrap.sh` (`/data/bootstrap.sh` in-container) and, if present, runs it as a bash script before binding its HTTP listener. Failure aborts boot. Only the container entrypoint runs it — lair (`--role lair`), or a standalone remote agent (`--role agent`) — because every local child shares lair's container and inherits whatever the script installs into the shared filesystem (`apt-get`, `npm i -g`, `uv tool install`, …). Locally-spawned children are marked with `OKTO_LOCAL_CHILD=1` and skip it. Operators set it up with `cp script ~/.okto/bootstrap.sh` rather than `okto env set STARTUP_SCRIPT=…`.
- **Remote-agent userdata writes the script to `/data/bootstrap.sh`.** `mint_bootstrap_userdata`'s `startup_script` argument now lands the script on the remote host at `/var/lib/okto/bootstrap.sh` (seen as `/data/bootstrap.sh` in the agent container) instead of injecting a `STARTUP_SCRIPT` env line. Behaviour from the operator's side is unchanged.

### Removed

- **The `STARTUP_SCRIPT` env var and per-agent `startup_script` argument.** `create_agent` / `spawn_agent` / `POST /agents` / `POST /agents/child` no longer accept `startup_script`; the agent supervisor no longer forwards a `STARTUP_SCRIPT` env to children. Per-agent startup scripts didn't make sense once all local agents share lair's container — use the shared `~/.okto/bootstrap.sh` instead (above). `mint_bootstrap_userdata` keeps its `startup_script` argument (now routed through the remote `/data/bootstrap.sh`).

- **Kaniko executor and its leaked env vars.** Replaced by Buildah (above). The image no longer COPYs from `gcr.io/kaniko-project/executor`, no longer sets `DOCKER_CONFIG=/kaniko/.docker/` or `SSL_CERT_DIR=/kaniko/ssl/certs`, and no longer carries the c_rehash workaround for the Kaniko cert bundle. OpenSSL clients (Python httpx, requests, Ruby `net/http`, curl) now use Debian's normal `/etc/ssl/certs/` setup straight out of the box, which fixes the `unable to get local issuer certificate` TLS failures that previously broke MCP servers like `mcp-proxy-for-aws` (JSON-RPC `-32602` during initialize) as a side effect.

### Fixed

- **Agent users now have home directories** (`useradd -m` instead of `-M`). Buildah looks up the calling user's home via `lstat(pw_dir)` early in its setup; with `-M` the home field in `/etc/passwd` pointed at `/home/okto-agent-N` but the directory didn't exist, so buildah aborted with `cannot resolve /home/okto-agent-N: lstat: no such file or directory` before any build step ran. With `-m`, each agent uid gets an empty `$HOME` populated from `/etc/skel`. (Lair still sets `HOME=/data/agents/<name>/` for the spawned agent process; this is purely about the `/etc/passwd` field that subprocess tooling reads independently.)

- **`newgidmap`/`newuidmap` now succeed for agent uids (matching primary gid).** Agents are created with `useradd -N` (no per-user group), so `/etc/passwd` records their primary gid as 100 (the `users` group). But lair's `uid_for_port` returns `(uid, uid)` and `cmd.uid().gid()` sets the spawned agent's process gid to match its uid (e.g. 10100/10100). `newgidmap` cross-checks `pw_gid` against the process's actual `st_gid` and refuses the mapping with `Target process is owned by a different user`, leaving buildah-rootless with only a single-uid fallback mapping — which then dies at `ApplyLayer ... remount /, flags: 0x44000: operation not permitted` during base-image extraction. Now the Dockerfile creates a per-user group with gid matching the uid via `groupadd -g $uid $name` + `useradd -g $uid -M -s …` so `pw_gid == st_gid` and `newgidmap` proceeds.

- **Base image bumped to `debian:trixie-slim` for buildah ≥ 1.39.** Bookworm shipped buildah 1.28.2, which has a hard-coded early `unshare(CLONE_NEWUSER)` in `containers/storage/pkg/unshare`'s init path — it runs before flag parsing, so `--isolation chroot` is examined too late, and Docker's default seccomp profile blocks the syscall for non-root callers (every agent build died with `Operation not permitted` regardless of `BUILDAH_ISOLATION=chroot` and the CAP_SYS_CHROOT we shipped in 0.16.2). Fixed upstream in containers/storage#1573, first released in buildah 1.30. Trixie (Debian 13, stable since mid-2025) ships buildah 1.39+ which respects `--isolation chroot` from the start. The builder stage also moves to `rust:1.88-slim-trixie` so the dynamically-linked libssl matches the runtime stage.

- **`buildah` works for non-root child agents (CAP_SYS_CHROOT granted as a file capability).** Out of the box, non-root agents had no working buildah isolation mode: `chroot` requires `CAP_SYS_CHROOT` which they don't have, and `rootless` requires `unshare(CLONE_NEWUSER)` which most container hosts (incl. stock Docker on Linux) deny with `Operation not permitted`. The image now runs `setcap cap_sys_chroot+ep /usr/bin/buildah` so the buildah binary executes with chroot capability regardless of the calling uid. Both lair (root) and agents (non-root) should now use `--isolation chroot`. The `buildah_note` system prompt and `docs/buildah.md` were updated accordingly.

- **Fixed misplaced `rootless_storage_path` in `/etc/containers/storage.conf`.** Was under `[storage.options]` (where it produced a benign `Failed to decode the keys` warning at every buildah invocation), now correctly under `[storage]`.

- **`buildah` no longer errors with "runroot must be set" out of the box.** The 0.16.0 image wrote a half-populated `/etc/containers/storage.conf` containing only `driver = "vfs"`. Buildah treats a partially-specified config as worse than no config and refuses to start rather than falling back to its built-in defaults — every `buildah …` invocation that didn't explicitly pass `--root`/`--runroot` flags died at config parse. The file now sets `runroot`, `graphroot`, and `rootless_storage_path = "$HOME/.local/share/containers/storage"` so the rootless path still lands under each agent's per-uid HOME.

- **Lair-side `${VAR}` MCP env/header expansion fails loudly.** Previously `expand_var` silently passed the literal `"${VAR}"` through to the child MCP process when neither `config.json` nor lair's process env had the variable, which surfaced later as opaque downstream errors (e.g. boto3 signing requests with the literal text "${AWS_ACCESS_KEY_ID}"). Now `connect_stdio` / `connect_http` abort with `[mcp] '<name>' initialize failed: env|header var(s) not set in lair container: …` before spawning, which the CLI's marker scanner renders as `HANDSHAKE FAILED — …` in `okto mcp add` / `okto mcp import` output.

## [0.11.5] - 2026-05-15

### Added

- **`send_notification` tool.** Both the lair and agent agentic loops now expose a `send_notification` tool so the model can push a notification to the operator's phone directly, rather than a push only happening as a side effect of background-task completion. Lair signs and POSTs to the relay itself; child agents (which hold no relay key) forward to lair's `/internal/notify`. Pushes use a distinct `agent_message` relay category.

## [0.11.0] - 2026-05-14

### Added

- **Agent-spawned-agent flow.** Children can now spawn their own children via two new tools (`spawn_agent`, `terminate_agent`), with ownership tracked on the `AgentRecord.parent` field. The mobile `agents` event surfaces `parent` so the sidebar can render the tree.
- **Cascade terminate.** `terminate_agent` (operator or agent) now BFS-terminates every transitive descendant leaves-first, kills the supervisor handles, drops registry rows + agent tokens, and removes per-agent data/workspace dirs. Operators no longer have to walk the tree manually.
- **Per-agent capability tokens.** When an agent spawns a child, lair mints a fresh random token, persists it at `/data/lair/agent-tokens.json` (0600 root-owned), and passes it to the child as `OCTO_AGENT_TOKEN`. The child uses it as `X-Octo-Agent-Token` against two new endpoints — `POST /agents/child` and `DELETE /agents/child/:name` — that are scoped: an agent can only spawn children of itself and only terminate agents it (transitively) spawned. Lair restarts adopt running children and re-issue their existing tokens.
- **Spawn caps.** `config.json` now accepts `agent_spawn_max_depth` (default 3) and `agent_spawn_max_descendants` (default 5) to bound runaway agent-spawned-agent trees. Operator-spawned agents are unrestricted.

### Security

- **Per-agent uids 10100..10199**, baked into `lair/Dockerfile`. `agent_proc::spawn` now maps loopback port → uid (`port 30100 → uid 10100`, …) so each child runs as its own uid. This closes one extra vector on top of 0.10.0: sibling agents could previously read each other's env (and so each other's `OCTO_AGENT_TOKEN`) via `/proc/<pid>/environ` because they shared uid 10001. The legacy uid 10001 / user `octo-agent` is kept as a fallback for the rare case where a non-standard port falls outside 30100..30199.

### Internal

- New `lair/src/agent_tokens.rs` module: persistent capability-token store with atomic 0600 writes.
- `core::Registry` grows `depth_of`, `direct_children`, and `descendants_leaves_first` helpers.
- `core::resolve_agent_spawn_caps` returns the (depth, descendants) pair from `Config`.
- `octo_core::AgentRecord` grows a `parent: Option<String>` field (`#[serde(default)]`, back-compat with existing `agents.json`).
- New `SpawnParams.agent_token` / `SpawnParams.lair_internal_url` fields propagate the per-agent token + lair API URL into the child's env.

## [0.10.3] - 2026-05-14

### Security

- **Remote agents now whitelist lair as the only legitimate Noise XX initiator.** `core::run_noise_proxy` (the responder loop, shared by both the lair role and the agent role) gains an `expected_initiator_pubkey: Option<Vec<u8>>` parameter. After the handshake, `core::handle_noise_connection` calls `session.get_remote_static()` and rejects with `initiator pubkey not on allowlist` when the bytes don't match. Without this, a third party who learned `(host, port, agent_pubkey)` could complete the Noise XX handshake against the remote agent and speak its protocol — Noise XX proves possession of the static key but doesn't bind it to an expected identity.
- The agent role (`lair/src/agent.rs`) now reads `LAIR_PUBKEY` from env at boot. If `AGENT_NOISE_PORT` is set (remote-agent mode) and `LAIR_PUBKEY` is unset or malformed, the role **refuses to start** — fail-closed. Local children (where `AGENT_NOISE_PORT` is unset) are unaffected.
- `mint_bootstrap_userdata` now embeds `LAIR_PUBKEY=<base32>` in the agent.env it writes, sourced from lair's current Noise static pubkey (`AppState.pubkey_b32`).
- The mobile-facing lair listener still passes `None` for the allowlist — that path is tracked separately by the "client-key allowlist + first-connection ack UI" item in `TODO.md`.

## [0.10.2] - 2026-05-14

### Fixed

- **Image now pins `NOISE_PORT=8443`** in `lair/Dockerfile`'s runtime ENV block. Prior 0.9.0 and 0.10.x images shipped with this unset, so lair fell back to its 9000 default inside the container while `EXPOSE` and the CLI's `-p` mapping both target 8443 — mobile's Noise handshake landed on an unbound port and HTTP-through-proxy failed with "Network request failed." Image-only change; the Rust code is unaffected.

## [0.10.1] - 2026-05-14

### Fixed

- **Remote-agent bootstrap (`mint_bootstrap_userdata` tool) was still wired for the deleted binary-release path.** The cloud-init it produced tried to `curl https://github.com/.../releases/download/cli-v<X>/octo-lair-linux-<arch>` — a path that no longer exists since 0.9.0 dropped the `lair-v*` binary artefacts in favour of the multi-arch Docker image. The userdata now:
  1. Trusts lair's operator SSH key.
  2. Installs Docker if absent (`curl https://get.docker.com | sh`), enables `docker.service`.
  3. `docker pull ghcr.io/georgebradford0/octo-lair:<lair_version>` (overridable via the new `image` arg on `mint_bootstrap_userdata`).
  4. Writes a systemd unit that `docker run`s the image with `--entrypoint /usr/local/bin/octo-lair`, `-v /var/lib/octo:/data`, `-p <public_port>:<public_port>`, and `--env-file /etc/octo/agent.env`.
- `ssh.rs::REMOTE_*_PATH` constants now point at the host-side bind-mount paths the container surfaces:
  - `REMOTE_AGENT_INFO_PATH` `/var/lib/octo/data/agent-info.json` → `/var/lib/octo/lair/agent-info.json` (container writes `/data/lair/agent-info.json`).
  - `REMOTE_CONFIG_PATH` `/var/lib/octo/data/config.json` → `/var/lib/octo/config.json` (container reads `/data/config.json` via `OCTO_HOME=/data`).
  - `REMOTE_WORKSPACE_PATH` unchanged at `/var/lib/octo/workspace`.
- Tool description + lair's system-prompt blurb for `mint_bootstrap_userdata` / `register_remote_agent` updated to reflect the docker-runtime flow.

## [0.10.0] - 2026-05-14

### Security

- **Child agents can no longer terminate lair, terminate sibling agents, or spawn new agents.** The four state-mutating management endpoints (`POST /agents`, `POST /agents/:name/start`, `POST /agents/:name/stop`, `DELETE /agents/:name`) now require an `X-Octo-Token` bearer header. The CLI mints the token on first `octo init`, persists it at `~/.octo/lair/.mgmt-token` (chmod 0600 on the host, root-owned 0600 inside the container), and passes it to lair via `docker -e LAIR_MGMT_TOKEN=<value>`. Lair reads the env var once at startup and removes it from the in-memory env.
- **Child agent processes drop to a non-root uid** (`octo-agent`, uid 10001, baked into `lair/Dockerfile`). `agent_proc::spawn` now sets `cmd.uid(10001).gid(10001)` and chowns each per-agent dir (`/data/agents/<name>/{,data,workspace}` and `agent.log`) to the agent uid before exec. This closes three vectors at once:
  - `kill 1` against lair from inside its bash tool — lair runs as root, child gets EPERM.
  - reading `LAIR_MGMT_TOKEN` from `/proc/1/environ` — owned by root, child gets EACCES.
  - reading `/data/config.json` or `/data/lair/*` if they're 0600 (host-owned via bind mount).
- `agent_proc::spawn` also explicitly `env_remove`s `LAIR_MGMT_TOKEN` from the child's `Command` env (belt+suspenders on top of the uid drop).
- Children now spawn with `HOME=/data/agents/<name>` so npm/uvx/gh/git caches land in a writable per-agent dir.

### Notes

- If `LAIR_MGMT_TOKEN` is unset at lair startup (e.g. ad-hoc `docker run` without the CLI), lair logs a warning and leaves the management endpoints open. Production deploys via `octo init` always supply one.
- The read-only / mobile-facing routes (`/health`, `/info`, `/history`, `/stream`, `/interrupt`, `/clear`, `/agents` GET, `/agents/:name/{history,interrupt,clear,branches,logs,stream}`) are not behind the token — mobile reaches them through the Noise tunnel and the CLI relies on `/agents` GET for `octo agents list`.

## [0.9.0] - 2026-05-14

### Changed

- **Distribution model: lair ships exclusively as a multi-arch Docker image (`ghcr.io/georgebradford0/octo-lair`).** The standalone `octo-lair-linux-{x86_64,aarch64}` binary release path (`lair-v*` tags, `octo lair update` binary download, `~/.octo/bin/octo-lair`) is gone. `octo init` `docker pull`s the image and `docker run`s it in detached mode with the operator's `~/.octo` bind-mounted at `/data` and `~/.octo/lair-env` ingested via `--env-file`.
- **The Rust code (CLI + lair) does not import a Docker SDK.** Every Docker interaction is a shell-out — either from `cli/src/service.rs` for container lifecycle (`run` / `rm -f` / `inspect` / `pull` / `logs`), or from the lair agentic loop's `bash` tool. No `bollard`, no `docker.rs` resurrection.
- **Children stay in-container.** Lair's `AgentSupervisor` still spawns each child as a plain `octo-lair --role agent` process via `tokio::process::Command` — inside the same container as lair. Children inherit the lair process env by default, so env vars passed via `docker run -e KEY=VAL` (or `octo env set KEY=VAL`) automatically reach every child agent and every MCP server they invoke.
- `OCTO_HOME=/data` is baked into the image so `config.json`, the SSH keypair, and the noise keypair resolve under the bind-mounted host dir. `OCTO_DATA_DIR=/data/lair` and `OCTO_AGENTS_DIR=/data/agents` follow the same scheme.

### Removed

- `~/.octo/bin/octo-lair` managed-binary path and the `lair-v*` release-asset downloader in the CLI.

### Added

- `lair/Dockerfile` (multi-stage builder + bookworm-slim runtime).
- `.github/workflows/lair.yml` (manual dispatch) — multi-arch buildx → `ghcr.io/<owner>/octo-lair:<version>` + `:latest`.
- `scripts/build-lair-image.sh` as a local fallback for the CI workflow.
- `octo init --image <ref>` and `octo lair update --image <ref>` for pinning a specific image. `$OCTO_LAIR_IMAGE` works as a global override.
- `lair-launch.json` gains an `image` field so `octo reload` reuses the same image without flags.

## [0.6.4] - 2026-05-12

### Changed

- **BREAKING — `gh_token` removed from `config.json`.** `GH_TOKEN` is now sourced exclusively from lair's process env (operator-supplied via `octo init --env GH_TOKEN=…`, or in dev forwarded from the host shell by `start_dev.sh`). Lair reads it via `std::env::var("GH_TOKEN")` when forwarding to children, cloning for remote agents, or shelling out to `gh` / `git`. The trade-off is explicit: `GH_TOKEN` now appears in `docker inspect lair`, unlike the config-mounted secrets. Existing `config.json` files that still carry a `gh_token` field deserialize fine (the field is silently dropped on the next write) — but it stops being honoured, so set the env var. The `octo` CLI's `--gh-token` flag is removed from `init` and `config set`.
- `${GH_TOKEN}` references in `mcp.json` now fall through to `std::env::var()` (the well-known-name mapping in `expand_var` was dropped for this one variable since the config field no longer exists).

### Fixed

- **Child agent containers were booting with `--role lair` instead of `--role agent`.** `docker_ops::create_agent_container` set `cmd:` in the bollard `ContainerConfig`, which Docker appends to the image's exec-form `ENTRYPOINT` rather than substituting. The effective child argv ended up as `/usr/local/bin/octo-lair --role lair /usr/local/bin/octo-lair --role agent`, so the parent role won and clap then errored on the trailing positionals. Switched to `entrypoint:` so the image's ENTRYPOINT is fully replaced.
- System prompt no longer suggests `bash docker …` for read-only diagnostics — the lair image doesn't ship a Docker CLI (orchestration goes through the typed tools), and the prior hint made the LLM mis-report `command not found` as "Docker isn't installed."

### Removed

- `awscli` is no longer installed in the runtime image — cloud provisioning is handled by MCP servers now, so the ~70 MB was dead weight.

## [0.6.3] - 2026-05-12

### Changed

- **`${VAR}` references in `mcp.json` env / header values now resolve against `/data/config.json` first, falling back to process env.** Well-known credential names map to config fields: `${ANTHROPIC_API_KEY}` → `config.anthropic_api_key`, `${OPENAI_API_KEY}` → `config.openai_api_key`, `${OPENAI_API_URL}` → `config.api_url`, `${MODEL}` → `config.model`, `${GH_TOKEN}` → `config.gh_token`. Other variable names still resolve from env (and fall back to the literal string). Without this, the 0.6.2 env-stripping broke mcp.json files that referenced API keys via `${VAR}`.
- CLI's `octo mcp add KEY=${VAR}` / `octo mcp import` now store `${VAR}` references verbatim instead of resolving them against the host env at write time. Resolution happens at MCP-server spawn time, so `octo config set --gh-token=...` rotates the value seen by downstream MCP servers on the next lair restart.

## [0.6.2] - 2026-05-12

### Changed

- **Credentials moved from env → `/data/config.json`.** Lair no longer reads `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENAI_API_URL`, `MODEL`, or `GH_TOKEN` from process env. They live in the operator's `~/.octo/config.json`, which the CLI now bind-mounts read-only at `/data/config.json` inside the lair container. `exec_create_agent`, `exec_mint_bootstrap_userdata`, and the git-clone code path read from `octo_core::read_config()` instead. Net effect: `docker inspect lair` no longer surfaces API keys, and credential rotation is live (just edit / `octo config set` — no `octo reload` needed since the resolver re-reads on every model call).
- `NOISE_PRIVATE_KEY` is also no longer injected via env. Lair falls back to the file-based keypair at `/data/noise_key.bin` (already persisted there since 0.5.x).

## [0.6.1] - 2026-05-11

### Changed

- System prompt now explicitly forbids the LLM from hand-writing userdata for the remote-agent flow. The lair image's default ENTRYPOINT is `--role lair`; only `mint_bootstrap_userdata`'s output appends `--role agent` and embeds the operator SSH pubkey. A hand-rolled userdata silently boots a second lair on the VM instead of an agent (causes a `register_remote_agent` timeout and SSH-auth failure).
- System prompt also surfaces the SSH key location (`/data/ssh_id_ed25519`) and the always-embedded-via-userdata invariant, so the LLM can `bash`-`ssh` into any provisioned remote agent for debugging.

### Changed

- **BREAKING — Kubernetes backend removed.** lair now uses the local Docker daemon (via `bollard`) to create, start, stop, and destroy agent containers. Per-agent Kubernetes Deployments / Services / PVCs are replaced by Docker containers + two named volumes (`agent-<name>-data`, `agent-<name>-workspace`). The `ChildVersion` POST endpoint and `DEPLOYMENT_NAME` env are gone; image versions are recorded in the new `agents.json` registry at create time.
- The `NOISE_KEY_FILE` default for the agent role moved from `/etc/octo/noise_key.bin` to `/data/noise_key.bin` to match lair's named-volume model.
- **Removed `message_lair` (agent) and `message_child` (lair) tools** along with the `LAIR_URL` env, agent-side reqwest pipeline, and the `host.docker.internal` extra-hosts entry on child containers. Lair is now a pure orchestrator; child-to-parent and parent-to-child messaging is gone.

### Added

- **Remote-agent flow**: `mint_bootstrap_userdata`, `register_remote_agent`, `forget_agent` lair tools, plus a new `lair/src/ssh.rs` that shells out to `openssh-client` for read-and-pull, write-file-via-stdin, and run-bash-via-stdin operations. The agent role now writes `/data/agent-info.json` at boot (pubkey, port, ready_at). Userdata is credentials-free; lair drops `/data/config.json` and runs an inline clone script over the SSH connection after the agent is up, then `docker restart`s the container so the agent's `ensure_workspace` re-runs against the freshly-cloned repo. `AgentRecord` gains `provider` and `metadata`; `is_remote()` distinguishes the two flavours so the docker poller doesn't drop remote rows. Each one-shot SSH op auto-retries up to 4× with exponential backoff; the registration tool inserts a `Pending` row as soon as the agent's identity is known and supports resume on a second call with the same name+host (every SSH phase is idempotent).

## [0.0.5] - 2026-03-28

### Added
- `GET /completions` endpoint for `@`-triggered file and worktree autocomplete in the mobile chat input

## [0.0.4] - 2026-03-28

### Changed
- `GH_TOKEN` can now be supplied via a mounted secret file (`/run/secrets/gh_token`) as a fallback when the env var is not set:
  ```
  --mount type=secret,id=gh_token,target=/run/secrets/gh_token
  ```

## [0.0.3] - 2026-03-28

### Fixed
- Entrypoint now exits immediately with a clear error if `GH_TOKEN` is unset and `GIT_URL` is an HTTPS URL, instead of silently attempting an unauthenticated clone and hanging

## [0.0.2] - 2026-03-28

### Changed
- Renamed `GIT_TOKEN` environment variable to `GH_TOKEN` for consistency with GitHub's own naming convention. Update any `docker run` invocations or `.env` files accordingly.

## [0.0.1] - 2026-03-28

### Added
- Noise_XX_25519_ChaChaPoly_SHA256 transport replacing SSH tunnel — QR code format changed from v1 (SSH) to v2 (Noise)
- MixHash fixes per Noise spec §5.2 (empty payload) and §5.6 (empty prologue)
- `create_pull_request` tool for opening GitHub PRs and GitLab MRs from the agentic loop
- Auto-detection of public IP via `api.ipify.org` at container startup
- Base32+colon QR payload format for alphanumeric QR mode (smaller, more reliable scan)
- Repo name surfaced in app header from git remote URL
- Multi-platform Docker image (linux/amd64, linux/arm64)
