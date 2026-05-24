# Env & Config in okto

How configuration values move from the operator to lair, to local child
agents, and to remote-VM agents. Read this when you're not sure *which*
file or env var something should live in, or why a key isn't being seen
by a particular process.

okto is post-Docker — every "container env var" intuition from the old
setup carries over, but the delivery mechanism is now plain process env
vars on native OS processes. See also @CLAUDE.md for the architectural
overview.

---

## Three storage locations

| Location | Format | Contents | Edited via |
|---|---|---|---|
| `~/.okto/config.json` | JSON | Credentials + model (`anthropic_api_key`, `openai_api_key`, `model`, `api_url`) | `okto config set` (or `okto init --…`) |
| `~/.okto/lair-env` | `KEY=VALUE\n` lines | Operator-supplied process env (`GH_TOKEN`, `BRAVE_API_KEY`, …) | `okto env set/unset KEY=VALUE` |
| `mcp.json` (per role) | JSON array | Per-MCP-server `env: {}` map | `okto mcp add --env K=V` |

Their lifetimes differ:

- **`config.json`** is read at every model call by `okto_core::resolve_api_key()` / `resolve_model()`. Rotation is **live**: edit it with `okto config set` and the next LLM request picks up the new value. No lair restart.
- **`lair-env`** is read once when lair spawns. `okto env set/unset` rewrites the file and **automatically restarts lair** to apply the change.
- **`mcp.json`** is watched by a 2-second mtime poller (`core/src/mcp.rs::start_mcp_watcher`). A change adds / removes MCP server processes without touching the parent role. `okto mcp add` waits up to 60 s for the new server's `[mcp] '<name>' connected` log line, rolls back on failure.

---

## The spawn chain

There are three spawns, each setting env on the child:

```
operator shell ──► okto (CLI) ──► lair --role lair ──► lair --role agent
                                                        ──► (or systemd on a VM)
```

### 1. CLI → lair

`cli/src/service.rs::start_lair` builds the lair process env explicitly:

```
OKTO_DATA_DIR=$HOME/.okto/lair
OKTO_AGENTS_DIR=$HOME/.okto/agents
NOISE_PORT=<--noise-port>
PUBLIC_PORT=<same>
OKTO_SKIP_SHELL_ENV=1
OKTO_LAIR_BINARY=<resolved path to lair>
```

Then it appends every `KEY=VALUE` pair from `~/.okto/lair-env`. The list of
*managed* keys (`OKTO_DATA_DIR`, `OKTO_AGENTS_DIR`, `NOISE_PORT`,
`PUBLIC_PORT`, `OKTO_SKIP_SHELL_ENV`, `OKTO_LAIR_BINARY`) is hard-coded
in `cli/src/init.rs::MANAGED_ENV_KEYS`; `okto env set` rejects attempts to
override them.

Lair inherits the CLI's process env (which includes the operator's shell
env) by default. The `OKTO_SKIP_SHELL_ENV=1` flag tells
`okto_core::init_shell_env()` to *not* re-source `~/.zshrc` / `~/.bashrc` —
no longer strictly necessary (lair already lives inside the operator's
shell tree) but kept for parity with the agent role and to avoid surprise
re-sourcing for users who have side-effecting login scripts.

### 2. Lair → local child agent

When the LLM calls `create_agent` (or `okto agents start` re-spawns a
stopped one), `lair/src/agent_proc.rs::spawn` sets on the child:

```
OKTO_DATA_DIR=$HOME/.okto/agents/<name>/data
WORKSPACE_DIR=$HOME/.okto/agents/<name>/workspace
AGENT_PORT=<assigned, 30100–30199>
OKTO_SKIP_SHELL_ENV=1
```

Plus the per-spawn arguments if the tool input included them:

```
GIT_URL          STARTUP_SCRIPT   STARTUP_PROMPT   AGENT_PURPOSE
```

Plus provider credentials, **explicitly forwarded** from lair's
`okto_core::read_config()` call:

```
ANTHROPIC_API_KEY   OPENAI_API_KEY   OPENAI_API_URL   MODEL
```

Plus `GH_TOKEN` if it was set in `lair-env`.

**Important: the child inherits lair's whole process env by default.**
`tokio::process::Command` doesn't `env_clear()` unless you ask it to. So
anything in `lair-env` reaches every local child transitively, not just
the keys explicitly listed in `SpawnParams`. This is how a custom env var
you set with `okto env set FOO=bar` flows down to MCP servers spawned
inside a child — no plumbing needed.

### 3. Lair → remote-VM agent (no inheritance)

Remote agents are bootstrapped fresh on a Linux VM. Nothing inherits;
there's no process tree connecting lair and the remote.
`exec_mint_bootstrap_userdata` produces a cloud-init script that writes
`/etc/okto/agent.env`:

```
AGENT_PORT=8000
AGENT_NOISE_PORT=9000           # presence flips on the agent's Noise responder
OKTO_DATA_DIR=/var/lib/okto/data
WORKSPACE_DIR=/var/lib/okto/workspace
NOISE_KEY_FILE=/var/lib/okto/data/noise_key.bin
OKTO_SKIP_SHELL_ENV=1
AGENT_PURPOSE=…                 # optional
STARTUP_SCRIPT=…                # optional
```

Loaded by systemd via `EnvironmentFile=` in `/etc/systemd/system/okto-agent.service`.

**Zero credentials in the userdata** by design — userdata ends up in
cloud-provider audit logs / IMDS, and you don't want API keys there.
Credentials arrive *after* first boot:

1. The agent process starts, sees no `config.json`, runs with no LLM
   ability — but does publish `agent-info.json` and serve `/health`.
2. `register_remote_agent` SSHes in (using lair's operator key) and drops
   the operator's `config.json` (anthropic_api_key, etc.) at
   `/var/lib/okto/data/config.json` over the SSH channel.
3. `systemctl restart okto-agent` so the agent re-reads it.

`GH_TOKEN` is *not* shipped to remote agents either. If you provide a
`git_url` to `register_remote_agent`, lair splices the token into the
clone URL on lair's side and pipes the script over SSH stdin — the token
appears only in the cloned repo's `.git/config` credential helper on the
remote, never as a process env var.

---

## API-key resolution precedence

In every role (lair, local agent, remote agent), `resolve_api_key()`
looks for the active provider's key in this order:

1. **Process env** (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`). Lair sets
   these explicitly on local children at spawn time.
2. **`~/.okto/config.json`** (read live on every call via `read_config()`).
3. **`~/.zshrc` / `~/.bash_profile` / etc.** — only consulted when
   `OKTO_SKIP_SHELL_ENV` is *not* set. Effectively never used in
   production since both roles set it.

Backend selection (Anthropic vs OpenAI-compatible) keys off
`OPENAI_API_URL` env > `config.api_url`. If either is set, the OpenAI
path is used and `OPENAI_API_KEY` becomes the preferred credential.

Model selection: `MODEL` env > `config.model` > `claude-sonnet-4-6`
default.

---

## MCP env

Per-MCP-server env lives inside each entry's `env: {}` map in `mcp.json`:

```json
[
  {
    "name": "github",
    "command": "npx",
    "args": ["-y", "@modelcontextprotocol/server-github"],
    "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_..." }
  }
]
```

When `okto mcp add --env KEY=VALUE` runs, any `${VAR}` references in
values are **expanded against the operator's shell env at write time**
(`cli/src/init.rs::expand_host_env`). The file always contains resolved
literals — the MCP server process gets the actual secret. Without this,
MCP servers spawned by lair (which doesn't share the operator's
interactive shell env) would see literal `${VAR}` strings.

Missing references abort the write with all missing vars listed at once:

```
$ okto mcp add --name brave --command npx --env BRAVE_API_KEY='${BRAVE_KEY}' -- @modelcontextprotocol/server-brave-search
error: env var(s) not set in this shell: BRAVE_KEY.
```

Hot-reload: `core/src/mcp.rs::start_mcp_watcher` polls the file's mtime
every 2 s and calls `reload_mcp_pool` on change, diffing the in-process
pool against the new list (adds new servers, drops removed ones). Adds
that fail to connect within 60 s get rolled back automatically by
`okto mcp add`.

---

## Practical cookbook

```sh
# Anthropic key — read live, no restart
okto config set --anthropic-api-key sk-ant-XXXX

# Switch to an OpenAI-compatible backend
okto config set --api-url https://api.openai.com/v1/chat/completions \
                --openai-api-key sk-XXXX --model gpt-4o

# GitHub token (lair restart)
okto env set GH_TOKEN=ghp_XXXX

# Brave search (turns on the `web_search` built-in tool — lair restart)
okto env set BRAVE_API_KEY=BSA-XXXX

# Add an MCP server with env (hot-reload)
okto mcp add --name linear --command npx \
  --env LINEAR_API_KEY=lin_api_XXXX \
  -- -y @linear/mcp-server

# Set a custom env on a specific child agent's MCP only
okto mcp add --agent lair-myrepo --name foo --command npx \
  --env CUSTOM_VAR='${HOST_SHELL_VAR}' -- @scope/foo

# Inspect the current state
okto config show                            # mask credentials, show model + url
okto env show                               # operator vars in ~/.okto/lair-env (masked)
okto mcp list --agent lair                  # MCP servers + their env keys
cat ~/.okto/config.json                     # raw
cat ~/.okto/lair-env                        # raw

# Force a non-default lair binary (dev / cross-compile testing)
OKTO_LAIR_BINARY=/path/to/lair okto init --anthropic-api-key sk-ant-…

# Force a non-default data dir (e.g. ./start_dev.sh sets this)
OKTO_DATA_DIR=$PWD/dev-data ./target/release/lair --role lair
```

---

## Quick reference: where each env var ends up

| Variable | Source | Used by |
|---|---|---|
| `ANTHROPIC_API_KEY` | `config.json` → spawn env (local) / `config.json` over SSH (remote) | LLM calls |
| `OPENAI_API_KEY` / `OPENAI_API_URL` / `MODEL` | Same | Same |
| `GH_TOKEN` | `lair-env` → spawn env (local only) | `bash gh …`, HTTPS git clone (via splice), MCP servers |
| `BRAVE_API_KEY` | `lair-env` → spawn env | Built-in `web_search` tool registration |
| `OKTO_DATA_DIR` | CLI (lair) / lair (children) | `okto_core::data_dir()` |
| `OKTO_AGENTS_DIR` | CLI (lair only) | `AgentSupervisor::new` |
| `OKTO_LAIR_BINARY` | CLI (lair only) | `AgentSupervisor` for child re-exec |
| `NOISE_PORT` / `PUBLIC_PORT` | CLI (lair only) | Lair's Noise responder |
| `AGENT_PORT` | Lair (children) / userdata (remote) | Agent's HTTP server bind |
| `AGENT_NOISE_PORT` | userdata (remote only) | Agent's optional Noise responder |
| `NOISE_KEY_FILE` | userdata (remote only) | Per-agent Noise keypair location |
| `WORKSPACE_DIR` | Lair (children) / userdata (remote) | `bootstrap::ensure_workspace` |
| `GIT_URL` | Per-spawn arg | `bootstrap::ensure_workspace` (initial clone) |
| `STARTUP_SCRIPT` | Per-spawn arg | `bootstrap::run_startup_script` |
| `STARTUP_PROMPT` | Per-spawn arg | Agent's first turn |
| `AGENT_PURPOSE` | Per-spawn arg | `build_agent_system_prompt` |
| `OKTO_SKIP_SHELL_ENV` | All managed roles | `init_shell_env()` short-circuit |
| `OKTO_DEV` | `./start_dev.sh` only | Pins dev Noise keypair + sets `PUBLIC_HOST=127.0.0.1` |
| `OKTO_RELAY_URL` | Operator (rare) | Override push-notification relay endpoint |

---

## What changed from the Docker setup

A few rough edges from the old flow are gone:

- **No more bind-mounted `config.json` at `/data/config.json`.** Config now lives at the same path on disk that every role reads (`~/.okto/config.json`); `okto_core::config_dir()` always resolves there regardless of `OKTO_DATA_DIR`, so lair / agents / CLI share it without inode-pinning gymnastics.
- **`docker inspect lair` leak surface is gone.** Operator env still goes to lair's process, but it only surfaces in `ps eww $(cat ~/.okto/lair/lair.pid)` for users with shell access. Same trust boundary, just one fewer way for it to leak.
- **`OKTO_AGENT_IMAGE` is retired.** Children re-exec the same binary as lair (`OKTO_LAIR_BINARY` resolves at lair startup).
- **Userdata env format is unchanged**, just consumed by systemd's `EnvironmentFile=` directive instead of Docker's `--env-file`.
