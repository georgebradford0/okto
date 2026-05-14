# Agent isolation inside the lair container

This doc explains how child agent processes are kept from terminating lair, terminating their siblings, or spawning new agents. As of `lair 0.10.0` / `cli 0.4.0`.

## Threat model

Lair is the parent. It spawns one `octo-lair --role agent` process per child agent (via `lair/src/agent_proc.rs::AgentSupervisor::spawn`). All of these live in the **same Docker container** — children don't get their own container. Every process has a `bash` tool exposed to its agentic loop, so we have to assume the LLM driving any process will eventually try to do something it isn't supposed to:

- A child kills `pid 1` and brings down the whole container.
- A child reads lair's secrets out of `/proc/1/environ` or `/data/config.json`.
- A child curls lair's management HTTP API to `POST /agents` (spawn) or `DELETE /agents/:name` (terminate sibling).
- A child curls a sibling's loopback HTTP server and disrupts it.

The first three are now blocked. The fourth is mostly inert (children's own HTTP only exposes interrupt/clear/etc.) but is called out at the bottom.

## What blocks each vector

### 1. Spawning / starting / stopping / deleting agents over HTTP

Lair's management HTTP server has four state-mutating routes:

- `POST   /agents`
- `POST   /agents/:name/start`
- `POST   /agents/:name/stop`
- `DELETE /agents/:name`

These are gated by an `axum` middleware that requires an `X-Octo-Token: <token>` header. The CLI mints the token on first `octo init` and persists it on the host at `~/.octo/lair/.mgmt-token` (chmod 0600). It's passed to the container with `docker run -e LAIR_MGMT_TOKEN=<token>` and sent on every CLI request as the header.

Lair reads `LAIR_MGMT_TOKEN` from its env once at startup, stores it in `AppState`, and immediately calls `std::env::remove_var("LAIR_MGMT_TOKEN")` so casual reads of `std::env::vars()` don't see it. The original value still lives in `/proc/1/environ` (Linux populates that from the initial exec and never updates it on `unsetenv`), so item 3 below is what actually keeps it out of children's reach.

The read-only and mobile-facing routes (`/health`, `/info`, `/history`, `/stream`, `/interrupt`, `/clear`, `/agents` GET, `/agents/:name/{logs,history,interrupt,clear,branches,stream}`) are **not** behind the token. Mobile reaches them through the Noise tunnel; `octo agents list` uses `GET /agents`.

When `LAIR_MGMT_TOKEN` is unset at startup (an ad-hoc `docker run` without the CLI), lair logs a warning and leaves the management routes open. Production deploys via `octo init` always supply one. If you want fail-closed defaults, set `LAIR_MGMT_TOKEN` to any non-empty string yourself before `docker run`.

### 2. `kill 1` against lair

`lair/Dockerfile` creates a non-root user:

```Dockerfile
RUN useradd -u 10001 -M -N -s /bin/bash octo-agent
```

`agent_proc::spawn` sets the spawned child's effective uid + gid to 10001 before exec:

```rust
cmd.uid(AGENT_UID).gid(AGENT_GID);
```

Lair itself stays at uid 0 (root) inside the container. Sending a signal across uids without `CAP_KILL` fails with `EPERM`, so the child can't `kill -TERM 1`. Same applies to killing siblings — every child runs as uid 10001, so all children share a uid, which means *one child can kill another sibling*. (See "Known residuals" below.)

### 3. Reading `/proc/1/environ`, `/data/config.json`, etc.

The Linux kernel restricts `/proc/<pid>/environ` (and `/proc/<pid>/auxv`, etc.) to readers with the same uid as the target process, or to root / `CAP_SYS_PTRACE`. With lair as root and children as 10001:

- `cat /proc/1/environ` → EACCES.
- `cat /data/config.json` → EACCES if the file is `0600` owned by root.
  - On a fresh `octo init` we don't currently chmod config.json inside the container, but it's also written from the host as `chmod 0600`, owned by the host user (uid 1000 typically). The bind-mount surfaces that file as uid 1000 with mode 0600 inside the container — different from the child's 10001, so EACCES.
- `cat /data/lair/*` (noise key, relay signing key, sessions, registry) → same story; lair writes these as root under `/data/lair`, the child running as 10001 can't read 0600 root-owned files.

The agent uid also can't read `/data/agents/<sibling>/...` of *other* children — wait, this is a wrinkle. Each child runs as the *same* uid (10001). Per-agent dirs are chowned to 10001 on spawn. So child A can read child B's data dir. (See "Known residuals.")

### 4. Where the child *can* write

Each child needs:

- A writable workspace (its git checkout or scratch dir).
- A writable data dir for `messages.json`, `mcp.json`, etc.
- A writable HOME so `gh`, `git`, `npm`, `uvx`, etc. can put their caches somewhere.

`agent_proc::spawn` handles all three:

```rust
chown_best_effort(&agent_dir,     AGENT_UID, AGENT_GID);
chown_best_effort(&data_dir,      AGENT_UID, AGENT_GID);
chown_best_effort(&workspace_dir, AGENT_UID, AGENT_GID);
chown_best_effort(&log_path,      AGENT_UID, AGENT_GID);
cmd.env("HOME", &agent_dir);
```

The chowns are best-effort because in dev mode (`start_dev.sh`) the lair binary may run as a non-root host uid, in which case chown returns EPERM. That's fine — in dev, lair and children run as the same uid anyway, so file permissions don't gate access.

## Wire flow at boot

```
operator host                      lair container
─────────────                      ──────────────
~/.octo/lair/.mgmt-token  ◄── mints once on `octo init`
                                   (CLI calls ensure_mgmt_token)

           docker run \
             --env-file ~/.octo/lair-env \
             -e PUBLIC_PORT=8443 \
             -e LAIR_MGMT_TOKEN=<token> ──►  process env (PID 1, root)

                                   lair main():
                                     read LAIR_MGMT_TOKEN → AppState
                                     std::env::remove_var(...)
                                     bind 0.0.0.0:8000

                                   AgentSupervisor::spawn(child):
                                     mkdir + chown /data/agents/<name>/...
                                     env_remove("LAIR_MGMT_TOKEN")
                                     cmd.uid(10001).gid(10001)
                                     exec /usr/local/bin/octo-lair --role agent

                                   child runs as uid 10001:
                                     - has its own /data/agents/<name>/data
                                     - HOME=/data/agents/<name>
                                     - cannot read /proc/1/environ
                                     - cannot kill pid 1
                                     - curl 127.0.0.1:8000/agents POST → 403
```

## Known residuals

These are intentionally **not** addressed in this round. They're either lower-impact or require a bigger architectural shift.

- **Child can interrupt or clear a sibling.** Every child's own HTTP server (loopback only, ports 30100–30199) exposes `POST /interrupt`, `POST /clear`, `PUT /config`. A child can `curl http://127.0.0.1:<sibling_port>/interrupt` to nuke a sibling's in-flight turn. Annoying, not catastrophic. Fix is to extend the same token middleware onto the child HTTP server, with the token forwarded only to lair (which already uses these endpoints via the `cli_*` proxy handlers).
- **Child can `kill <sibling_pid>` of a sibling.** Same uid → same signal permissions. The fix here is a per-child uid (e.g. 10001 + agent ordinal), which complicates `/data/agents/<name>/` ownership and chown bookkeeping. Doable, not done.
- **Child can read another child's `/data/agents/<sibling>/` files.** Same uid means the 0700 dir perms don't help. Same fix as above (per-child uid).
- **Child can `cat /proc/1/cmdline`, `/proc/1/status`, etc.** These don't expose the token, just metadata. Not a real leak.
- **Child can reach the operator's host network if Docker's default bridge has egress.** This is a generic LLM-can-curl-anywhere problem, not specific to multi-agent setups.

## Source pointers

- `lair/Dockerfile` — creates `octo-agent` (uid 10001).
- `lair/src/lair.rs` — `mgmt_token` field on `AppState`; `require_mgmt_token` middleware; `protected` router merged into the main app.
- `lair/src/agent_proc.rs` — `AGENT_UID` / `AGENT_GID` constants; `chown_best_effort`; `spawn` sets `cmd.uid(...)`, `cmd.env_remove("LAIR_MGMT_TOKEN")`, `cmd.env("HOME", agent_dir)`.
- `cli/src/service.rs` — `ensure_mgmt_token` / `read_mgmt_token` / `mgmt_token_path` / `random_hex`. `start_lair` passes `-e LAIR_MGMT_TOKEN=...`.
- `cli/src/agents.rs` — `mgmt_request` helper attaches the `X-Octo-Token` header to every state-mutating call.
