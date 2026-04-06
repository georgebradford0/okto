# Runtime MCP Tools

Claudulhu supports adding tools at runtime — without rebuilding the Docker image — via the [Model Context Protocol (MCP)](https://modelcontextprotocol.io). MCP servers are external processes that advertise tools over a simple JSON-RPC protocol. Claudulhu acts as an MCP host: it spawns configured servers on startup, discovers their tools, and makes them available to the AI alongside built-in tools.

## How it works

1. On startup, Claudulhu reads `/data/mcp.json` (see [Configuration](#configuration)).
2. Each configured server is spawned as a child process.
3. Claudulhu performs the MCP handshake (`initialize` → `tools/list`) and merges the returned tool definitions into the set sent to Claude.
4. When Claude calls one of these tools, Claudulhu dispatches a `tools/call` JSON-RPC request to the appropriate server and returns the result.

The transport is **stdio** (newline-delimited JSON-RPC over the process's stdin/stdout). This is the standard transport used by all major MCP SDKs.

## Configuration

Create a file at `/data/mcp.json` containing a JSON array of server descriptors:

```json
[
  {
    "name": "github",
    "command": "npx",
    "args": ["-y", "@modelcontextprotocol/server-github"],
    "env": {
      "GITHUB_PERSONAL_ACCESS_TOKEN": "${GH_TOKEN}"
    }
  },
  {
    "name": "linear",
    "command": "npx",
    "args": ["-y", "@linear/mcp"],
    "env": {
      "LINEAR_API_KEY": "lin_api_xxxxxxxxxxxx"
    }
  },
  {
    "name": "my-custom-tool",
    "command": "/data/tools/my_server.py",
    "args": [],
    "env": {}
  }
]
```

### Fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Logical name used in log messages. Does not need to match the server's own `serverInfo.name`. |
| `command` | yes | Executable to run. Can be an absolute path or anything on `$PATH` (`npx`, `python`, `node`, …). |
| `args` | no | List of arguments to pass to the command. Defaults to `[]`. |
| `env` | no | Extra environment variables to set for the server process. Merged on top of the inherited environment. |

### Environment variable interpolation

Values in the `env` map of the form `"${VAR}"` are substituted from the host environment at startup time. This lets you pass secrets (API keys, tokens) that are already available as container environment variables without hardcoding them in the JSON file.

```json
"env": {
  "GITHUB_PERSONAL_ACCESS_TOKEN": "${GH_TOKEN}"
}
```

Interpolation only applies to the exact pattern `${VAR}` (the entire string must be `${…}`). Partial substitution (e.g. `"prefix_${VAR}"`) is not supported — use a wrapper script instead.

## Mounting the config file

### Docker run

```sh
docker run \
  -v /host/path/to/mcp.json:/data/mcp.json:ro \
  -e ANTHROPIC_API_KEY=... \
  -e GIT_URL=... \
  ghcr.io/georgebradford0/claudulhu-server:latest
```

### Docker Compose

Add a bind mount to your `docker-compose.yml`:

```yaml
services:
  claudulhu:
    image: ghcr.io/georgebradford0/claudulhu-server:latest
    volumes:
      - ./mcp.json:/data/mcp.json:ro
      - claudulhu-data:/data
    environment:
      - GIT_URL=${GIT_URL}
      - ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY}
      - GH_TOKEN=${GH_TOKEN}
```

> **Note:** If you use a named volume for `/data` (to persist the Noise keypair across restarts), make sure your `mcp.json` is also accessible at that path, either via a separate bind mount (`:ro` above) or by placing the file inside the named volume before startup.

## npm-based MCP servers

The Docker image includes `node` and `npm`, so `npx` is available out of the box. You can run any npm-published MCP server without pre-installing it:

```json
{
  "name": "github",
  "command": "npx",
  "args": ["-y", "@modelcontextprotocol/server-github"],
  "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GH_TOKEN}" }
}
```

The `-y` flag tells `npx` to install the package automatically if it is not cached. On first run this adds a few seconds of startup time per server. To avoid this you can pre-install packages into the image or mount a pre-populated `node_modules` directory.

## Writing a custom MCP server

Any process that implements the MCP stdio protocol can be used. The minimal surface you need to implement is:

| Method | Direction | Purpose |
|--------|-----------|---------|
| `initialize` | host → server | Capability negotiation |
| `notifications/initialized` | host → server | Signals handshake complete |
| `tools/list` | host → server | Returns tool definitions |
| `tools/call` | host → server | Executes a tool |

### Minimal Python example

```python
#!/usr/bin/env python3
"""Minimal MCP server — exposes a single `add` tool."""
import sys, json

def respond(id, result):
    print(json.dumps({"jsonrpc": "2.0", "id": id, "result": result}), flush=True)

TOOLS = [{
    "name": "add",
    "description": "Add two integers together.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "a": {"type": "number"},
            "b": {"type": "number"}
        },
        "required": ["a", "b"]
    }
}]

for line in sys.stdin:
    msg = json.loads(line)
    method, id_ = msg.get("method"), msg.get("id")
    if method == "initialize":
        respond(id_, {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "example", "version": "0.1"}})
    elif method == "notifications/initialized":
        pass  # no response for notifications
    elif method == "tools/list":
        respond(id_, {"tools": TOOLS})
    elif method == "tools/call":
        args = msg["params"]["arguments"]
        result = args["a"] + args["b"]
        respond(id_, {"content": [{"type": "text", "text": str(result)}]})
```

Save as `/data/tools/my_server.py`, make it executable (`chmod +x`), then add to `mcp.json`:

```json
{
  "name": "example",
  "command": "/data/tools/my_server.py",
  "args": [],
  "env": {}
}
```

Official MCP SDKs are available for Python, TypeScript, Kotlin, and Swift — see [modelcontextprotocol.io](https://modelcontextprotocol.io) for details.

## Ecosystem servers

A growing catalogue of ready-made MCP servers is available. Some popular ones:

| Server | npm package | What it provides |
|--------|-------------|-----------------|
| GitHub | `@modelcontextprotocol/server-github` | Repo search, file read, issue/PR management |
| Filesystem | `@modelcontextprotocol/server-filesystem` | Read/write arbitrary local paths |
| Postgres | `@modelcontextprotocol/server-postgres` | Query a PostgreSQL database |
| Brave Search | `@modelcontextprotocol/server-brave-search` | Web search via Brave API |
| Fetch | `@modelcontextprotocol/server-fetch` | HTTP fetch with content extraction |
| Linear | `@linear/mcp` | Linear issue and project management |

See the [MCP server registry](https://github.com/modelcontextprotocol/servers) for a full list.

## Troubleshooting

**Server fails to start**
Check container logs for lines beginning with `[mcp]`. The stderr of each MCP server process is forwarded to the container's stderr, so server-level errors appear there too.

**No tools are added**
Verify that `/data/mcp.json` is valid JSON and that `command` is on `$PATH` (or is an absolute path). Run `docker exec <container> cat /data/mcp.json` to confirm the file is mounted correctly.

**`npx` package not found**
On first run, `npx -y` downloads the package into a cache. If the container has no internet access, pre-install by adding a `RUN npm install -g <package>` layer to a custom `Dockerfile` that extends the base image.

**Tool name conflicts**
If a custom MCP tool has the same name as a built-in tool, the built-in takes precedence. Rename the custom tool to avoid collisions.
