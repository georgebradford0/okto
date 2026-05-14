# Changelog

## [Unreleased]

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
