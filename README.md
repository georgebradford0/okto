# octo

`octo` is a mobile agent management system that runs a fleet of LLM agents as plain OS processes on a Linux host. It was originally designed for coding but can be used to deploy any type of agent.

## Architecture

A single `octo-lair` binary runs on a host with a static IP. The mobile client connects to lair over an encrypted Noise tunnel; from the lair chat the user creates, messages, and tears down "child" agents. Each child is also an `octo-lair` process (spawned by lair with `--role agent`), listening on a loopback HTTP port. Mobile chats with a child by opening a WebSocket to lair's proxy URL (`/agents/<name>/stream`) — there is no separate connection per child.

Linux only (x86_64 and aarch64). No Docker, no systemd dependency.

## Install the CLI and lair binary

```sh
curl -fsSL https://raw.githubusercontent.com/georgebradford0/octo/main/scripts/get-cli.sh | sh
```

This installs `octo` (CLI) and `octo-lair` (the lair / agent binary) to `~/.local/bin/`. Both must be on PATH; if `~/.local/bin` isn't in your PATH, add `export PATH="$HOME/.local/bin:$PATH"` to your shell rc.

## Setup

`octo init` must be run on a Linux host with a static — or at least publicly-reachable — IP. It expects either `--anthropic-api-key` or `--openai-api-key` plus `--model`.

```sh
octo init --anthropic-api-key sk-ant-... --model claude-sonnet-4-6
```

`init` will:

1. Generate a Noise keypair and an Ed25519 SSH keypair (the SSH key is reserved for ops backchannels — e.g. SSHing into a remote host for tailing logs).
2. Write an env file (`~/.octo/lair-env`) and persist credentials to `~/.octo/config.json`.
3. Spawn `octo-lair --role lair` as a detached background process (pid recorded at `~/.octo/lair/lair.pid`).
4. Wait for lair's health check, then print a QR code containing the host, port, and Noise pubkey.

Anyone with the QR data can connect, so treat it like a credential.

The `mobile/` directory contains a React Native app (TODO: store links). Open the app, tap the pulsing icon, and scan the QR. The connection opens to the lair chat.

## Day-to-day

| Command | What it does |
|---|---|
| `octo init` | First-run setup. Spawns lair. |
| `octo reload` | Restart lair (picks up new env / binary). `--all` also restarts every agent. |
| `octo destroy` | Kill lair, terminate every agent, wipe `~/.octo/lair` and `~/.octo/agents`. |
| `octo logs [name]` | Tail `~/.octo/lair/lair.log` (default) or a specific agent's `agent.log`. `-f` to follow. |
| `octo agents list` | Show every known agent (status, pid, port, git URL). |
| `octo agents start <name>` | Re-spawn a stopped agent. |
| `octo agents stop <name>` | SIGTERM an agent. |
| `octo agents delete <name>` | Stop the agent and remove its `~/.octo/agents/<name>/` dir. |
| `octo config show` / `set` | View / edit `~/.octo/config.json` (API keys, model). Lair re-reads on every call — no restart needed. |
| `octo env show` / `set` / `unset` | View / edit `~/.octo/lair-env` (extra env vars passed to lair). Changes auto-restart lair. |

## MCP Support

MCP servers can be seeded at init time by passing an MCP JSON file:

```sh
octo init --mcp-config <path_to_mcp_json>
```

They can also be added at runtime and are hot-reloaded:

```sh
# uvx-based server
octo mcp add --name aws-ec2 --command uvx \
  --env AWS_ACCESS_KEY_ID=... --env AWS_SECRET_ACCESS_KEY=... --env AWS_REGION=us-east-1 \
  -- awslabs.amazon-ec2-mcp-server

# Add to a specific child agent (default is lair)
octo mcp add --agent lair-myrepo --name linear --command npx \
  --env LINEAR_API_KEY=lin_api_... \
  -- -y @linear/mcp-server

octo mcp list
octo mcp remove --name github
```

`mcp add` waits for the server to connect and reports the result. On failure the entry is automatically removed. Server configs are stored at:

- `~/.octo/lair/mcp.json` (for lair)
- `~/.octo/agents/<name>/data/mcp.json` (for child agents)

Both are hot-reloaded within a few seconds.

## Startup Scripts

New child agents are created via the built-in `create_agent` tool in the lair chat. It accepts `startup_script` (runs before the agent's HTTP server starts — good for `apt-get`, git config) and `startup_prompt` (the first message the agent receives once ready).

Both fields are stored as plaintext env on the agent process — they should not contain sensitive data.
