# CLI reference

Every `okto` command and flag. Run `okto <command> --help` on the host for the
authoritative, version-matched help text.

!!! abstract "Conventions"
    - `<required>` · `[optional]` · `...` = repeatable.
    - Commands that mutate a running agent reach lair's **loopback** management
      API (`127.0.0.1:8000`); the rest edit host files under `~/.okto`.

## Lifecycle

### `okto init`
Bootstrap lair as a Docker container on this host. Prompts for API keys / URL /
model the first time; if `~/.okto/config.json` already exists it **reuses** it
(no prompts) and just (re)starts lair. Safe to re-run.

| Flag | Default | Description |
|------|---------|-------------|
| `-e, --env KEY=VALUE ...` | — | Extra env var for the container; inherited by child agents. |
| `--noise-port <PORT>` | `8443` | Host-side Noise port the QR advertises. |
| `--http-port <PORT>` | `8000` | Loopback management-API port. |
| `--image <REF>` | `…/lair:latest` | Lair image reference. |
| `--mcp-config <PATH>` | — | Seed MCP servers from an `mcp.json`. |
| `--system-prompt-append <TEXT or @PATH>` | — | Append text to lair's system prompt. |
| `--disable-push` | off | Disable push notifications end-to-end. |
| `--ready-timeout <SECS>` | `1200` | Seconds to wait for health after `docker run`. |

### `okto reload`
Restart lair to apply env/config; optionally upsert env vars and pick agents.

| Flag | Default | Description |
|------|---------|-------------|
| `--agents <NAME> ...` | all | Restart only these agents. |
| `-e, --env KEY=VALUE ...` | — | Upsert env vars into `lair-env` before restart. |
| `--ready-timeout <SECS>` | `1200` | Seconds to wait for health. |
| `--check-config` | — | Validate config instead of restarting (see below). |

With `--check-config`, lair is **not** restarted. Instead okto validates the
effective configuration — the values in `~/.okto/config.json` overlaid with any
matching `~/.okto/lair-env` overrides (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`MODEL`, `OPENAI_API_URL`, `ANTHROPIC_API_URL`) — and then sends a minimal
one-token "ping" request to the configured backend (Anthropic or an
OpenAI-compatible API) to confirm the key, model, and URL actually work. It
exits non-zero on the first problem, so it's useful as a preflight before
`okto reload` or in scripts.

### `okto destroy`
Stop lair, remove every agent, and wipe lair's data dirs + env file + launch
record (keeps `config.json`).

| Flag | Description |
|------|-------------|
| `-y, --yes` | Skip the confirmation prompt. |

### `okto qr`
Print the QR code mobile clients scan to connect.

| Flag | Description |
|------|-------------|
| `--host <HOST>` | Override the advertised host (else `PUBLIC_HOST`, else auto-detected IP). |

### `okto logs [name]`
Show logs for lair (default) or a named agent.

| Arg / Flag | Description |
|------------|-------------|
| `[name]` | Agent name; omit for lair. |
| `-f, --follow` | Follow output. |

## CLI self-management

### `okto version`
Print the CLI version.

### `okto update`
Update the CLI to the latest release and refresh completions.

### `okto uninstall`
Remove the okto binary and shell completions. `-y` skips the prompt.

### `okto completions <shell>`
Print a completion script (`bash`, `zsh`, `fish`, `elvish`, `powershell`) to stdout.

## Runtime image

### `okto lair update`
Pull the latest lair image, restart the container, and respawn agents that were
running.

| Flag | Description |
|------|-------------|
| `--image <REF>` | Image to pull (else the recorded image, `$OKTO_LAIR_IMAGE`, then default). |

### `okto lair version`
Print the version of the **running** lair binary (requires lair running).

## Agents — `okto agents`

| Command | Description |
|---------|-------------|
| `okto agents list` | List agents (id, name, kind, status, port, pid, host). Reads `agents.json`; works offline. |
| `okto agents start <id\|name>` | Start a stopped agent. |
| `okto agents stop <id\|name>` | Stop a running agent. |
| `okto agents delete <id\|name> [-y]` | Delete an agent and its data/workspace (irreversible; `-y` skips prompt). |

> Agents are **created from the mobile chat**, not the CLI. See [Agents](agents.md).
>
> Each agent has a free-form **name** (may contain spaces) and a route-safe
> **id** (a slug derived from the name, also its on-disk dir name). `start` /
> `stop` / `delete` and `okto tasks --agent` accept either the id or an
> unambiguous name.

## MCP servers — `okto mcp`

All accept `--agent <name>` (default `lair`).

| Command | Description |
|---------|-------------|
| `okto mcp list` | List configured MCP servers. |
| `okto mcp add --name <n> --command <cmd> [--env K=V]... [-- <args>...]` | Add a server; waits for it to connect, rolls back on failure. |
| `okto mcp remove <name>` | Remove a server (hot-reloaded). |
| `okto mcp import <file>` | Replace config from a JSON file; validates + waits for new servers. |

See [MCP servers](mcp.md) for details and examples.

## Credentials & model — `okto config`

| Command | Description |
|---------|-------------|
| `okto config show` | Print config with secrets masked. |
| `okto config set [flags]` | Update fields (live-reloaded by lair). |

`okto config set` flags: `--model`, `--api-url`, `--anthropic-api-key`,
`--openai-api-key`, `--system-prompt-append <TEXT or @PATH>` (`""` clears),
`--cost-input1m <USD>`, `--cost-output1m <USD>` (negative clears).

## Env vars — `okto env`

| Command | Description |
|---------|-------------|
| `okto env show` | Print operator env vars (reserved keys hidden, values masked). |
| `okto env set KEY=VALUE ...` | Upsert env vars, then restart lair. |
| `okto env unset KEY ...` | Remove env vars, then restart lair. |

Reserved (managed for you): `NOISE_PORT`, `PUBLIC_PORT`, `OKTO_HOME`,
`OKTO_DATA_DIR`, `OKTO_AGENTS_DIR`, `OKTO_SKIP_SHELL_ENV`, `OKTO_LAIR_BINARY`,
`HOME`.

## SSH identity — `okto ssh`

| Command | Description |
|---------|-------------|
| `okto ssh pubkey` | Print the container's SSH public key (register it on GitHub, GPU pods, etc.). |

## Background tasks — `okto tasks`

| Command | Description |
|---------|-------------|
| `okto tasks list [--agent <name>]` | List tasks (aggregates across lair + agents by default). |
| `okto tasks stop <id> [--agent <name>]` | Cancel a running task; reports `fired: true/false`. |

## Defaults

| Setting | Default |
|---------|---------|
| Noise port (public) | `8443` |
| Management API port (loopback) | `8000` |
| Health wait | `180` s |
| Container name | `lair` |
| Image | `ghcr.io/georgebradford0/lair:latest` (override via `$OKTO_LAIR_IMAGE` or `--image`) |
| Default model | `claude-sonnet-4-6` |
