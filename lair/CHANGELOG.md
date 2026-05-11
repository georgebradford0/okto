# Changelog

## [Unreleased]

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
