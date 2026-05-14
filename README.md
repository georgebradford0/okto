# octo

`octo` is a mobile agent management system that runs a fleet of LLM agents inside a single Docker container on a Linux host. It was originally designed for coding but can be used to deploy any type of agent.

## Architecture

A single `octo-lair` container runs on a host with a static IP. The mobile client connects to lair over an encrypted Noise tunnel; from the lair chat the user creates, messages, and tears down "child" agents. Each child is another `octo-lair` process (spawned by lair with `--role agent`) inside the same container, listening on a loopback HTTP port. Mobile chats with a child by opening a WebSocket to lair's proxy URL (`/agents/<name>/stream`) — there is no separate connection per child, and no second container per agent.

Linux only (x86_64 and aarch64). The lair image is multi-arch (linux/amd64, linux/arm64). The Rust code never talks to the Docker daemon — every Docker interaction is either an `octo` CLI shell-out or a `bash` tool call from the agentic loop.

## Install the CLI

```sh
curl -fsSL https://raw.githubusercontent.com/georgebradford0/octo/main/scripts/get-cli.sh | sh
```

This installs the `octo` CLI to `~/.local/bin/octo`. Docker must already be installed and runnable as the current user — `octo init` `docker pull`s `ghcr.io/georgebradford0/octo-lair:latest` on first run.

## Setup

`octo init` must be run on a Linux host with Docker installed and a static — or at least publicly-reachable — IP. On first run it prompts interactively for credentials; on subsequent runs (when `~/.octo/config.json` already exists) it refuses to overwrite the existing config.

```sh
octo init
```

It prompts for:

- **Anthropic API key** — press Enter to skip.
- **OpenAI API key** — press Enter to skip. At least one of the two keys is required.
- **API URL** — Enter for the Anthropic default; otherwise the full chat-completions URL (e.g. `https://api.deepinfra.com/v1/openai/chat/completions`).
- **Model** — e.g. `claude-sonnet-4-6`.

`init` will then:

1. Persist credentials to `~/.octo/config.json`.
2. Generate a Noise keypair and an Ed25519 SSH keypair (the SSH key is reserved for ops backchannels — e.g. SSHing into a remote host for tailing logs).
3. Write an env file (`~/.octo/lair-env`) — this is what `docker --env-file` ingests.
4. `docker pull` the lair image, then `docker run -d --name octo-lair -v ~/.octo:/data -p 8443:8443 …`.
5. Wait for the management API on `127.0.0.1:8000/health`, then print a QR code containing the host, port, and Noise pubkey.

Pass `--image ghcr.io/you/octo-lair:0.9.0` (or `OCTO_LAIR_IMAGE=…`) to pin a specific image.

### Env vars

Anything passed via `--env KEY=VAL` (or stored later with `octo env set KEY=VAL`) is written to `~/.octo/lair-env` and ingested by docker on container start. **The same variables are inherited by every child agent process lair spawns** — child agents share the lair container's env, so a single `-e GH_TOKEN=…` reaches lair, every child, and any MCP server they invoke.

Anyone with the QR data can connect, so treat it like a credential.

The `mobile/` directory contains a React Native app (TODO: store links). Open the app, tap the pulsing icon, and scan the QR. The connection opens to the lair chat.

## Day-to-day

| Command | What it does |
|---|---|
| `octo init` | First-run setup. Pulls the lair image and `docker run`s it. |
| `octo reload` | Restart the lair container (picks up new env / image). `--all` also restarts every agent. |
| `octo destroy` | Remove the lair container, terminate every agent, wipe `~/.octo/lair` and `~/.octo/agents`. |
| `octo logs [name]` | `docker logs` for lair (default) or tail a specific agent's `agent.log`. `-f` to follow. |
| `octo lair update [--image …]` | `docker pull` the lair image and restart the container. |
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
