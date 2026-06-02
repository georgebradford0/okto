# MCP servers

[Model Context Protocol](https://modelcontextprotocol.io/) servers add extra
tools to an agent's tool list. lair (and each child agent) loads its servers
from an `mcp.json`; the CLI edits that file and hot-reloads the agent.

Commands default to **lair**; pass `--agent <name>` to target a child.

!!! note "When you actually need MCP"
    okto bakes in `gh`/`glab`/`git` and a full shell, so a lot of work needs no
    MCP at all — set an env var (`okto env set GH_TOKEN=…`) and let the model use
    the CLI. MCP shines for things with no good command-line client, e.g. a
    **cloud-provider MCP** so lair can provision [remote agents](agents.md#local-vs-remote-agents).

## Seed at init

```sh
okto init --mcp-config ./mcp.json
```

## List

```sh
okto mcp list                       # lair's servers
okto mcp list --agent lair-myrepo   # a child's servers
```

Prints each server's name, command, args, and env.

## Add

```sh
okto mcp add --name <name> --command <cmd> [--env KEY=VALUE]... [-- <args>...]
```

- `--name` *(required)* — server name.
- `--command` *(required)* — executable to spawn (e.g. `uvx`, `npx`, an absolute path).
- `--env KEY=VALUE` *(repeatable)* — env vars; `${VAR}` is expanded from your host
  shell **at add time** (a missing var is an error).
- `--agent <name>` — target a child agent (default `lair`).
- Everything after `--` is passed as positional **arguments** to the command.

!!! example
    ```sh
    # uvx-based server
    okto mcp add --name aws-ec2 --command uvx \
      --env AWS_ACCESS_KEY_ID=... --env AWS_SECRET_ACCESS_KEY=... --env AWS_REGION=us-east-1 \
      -- awslabs.amazon-ec2-mcp-server

    # npx-based server, scoped to a child agent
    okto mcp add --agent lair-myrepo --name linear --command npx \
      --env LINEAR_API_KEY=lin_api_... \
      -- -y @linear/mcp-server
    ```

`add` upserts the server, waits for it to report **connected** in the agent's
logs (up to ~60s), and **rolls back** if it fails to start.

## Remove

```sh
okto mcp remove <name>
okto mcp remove --agent lair-myrepo <name>
```

Hot-reload picks up the removal; no wait.

## Import

Replace an agent's config from a JSON file (array of server objects):

```sh
okto mcp import ./mcp.json
okto mcp import --agent lair-myrepo ./mcp.json
```

Import validates that each stdio server's command exists inside the lair
container, expands `${VAR}` in env/headers, waits for **new** servers to connect,
and rolls back on failure. Servers already present (by name) are skipped.

## Inheritance

When lair spawns a child agent, the child **inherits a snapshot of lair's
`mcp.json`** at creation time. Later edits to lair's config do **not** propagate
to existing children — use `okto mcp add --agent <name>` to change a specific
child. Per-agent edits survive restarts.

!!! info "Local agents only (for now)"
    The CLI can currently edit MCP config for **local** agents only.
