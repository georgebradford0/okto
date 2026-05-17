# Built-in Tools

The tools the agentic loop exposes to Claude without any MCP servers configured.
"Built-in" here means compiled into the Rust binary — for runtime-added tools, see
[Runtime MCP Tools](mcp-tools.md).

Which tools are visible depends on the role (`lair` vs. child `agent`) and on a few
runtime conditions (see [Conditional tools](#conditional-tools)).

## Common tools (every role)

Defined in [`core/src/lib.rs`](../core/src/lib.rs) `tool_definitions()`. Available to
both lair and every child agent.

| Tool | Purpose |
|------|---------|
| `bash` | Run a shell command in the working directory; returns stdout/stderr. |
| `read_file` | Read a file, with `offset`/`limit` for reading only a section. |
| `edit_file` | Replace an exact, unique string in an existing file. Preferred over `write_file` for edits. |
| `write_file` | Write a file. For creating new files only. |
| `glob` | Find files matching a glob pattern (e.g. `src/**/*.rs`). |
| `grep` | Search file contents for a regex; returns `file:line` matches. |
| `web_fetch` | Fetch a URL and return its text content (HTML stripped, truncated at 50 000 chars). |

### Background / notification tools

Registered for both roles via their extra-tool lists. Defined in
[`core/src/background.rs`](../core/src/background.rs) and
[`core/src/relay.rs`](../core/src/relay.rs).

| Tool | Purpose |
|------|---------|
| `run_command_in_background` | Run a long shell command in the background; output is injected back into the conversation when it finishes. |
| `monitor_process` | Run a long process as a background task and get woken with its output *while* it runs, so the model can react mid-run. |
| `send_notification` | Send a push notification to the operator's phone via the relay. Best-effort; a no-op if no relay is configured. |

## Lair-only tools

Defined in [`lair/src/lair.rs`](../lair/src/lair.rs) `lair_extra_tools()`. Only the
parent `lair` process sees these — they manage the agent fleet.

| Tool | Purpose |
|------|---------|
| `list_agents` | List every known agent (local + remote) with its full registry row. |
| `create_agent` | Spawn a new local child agent as an OS process on the lair host. |
| `mint_bootstrap_userdata` | Mint a credential-free cloud-init script for bootstrapping a remote agent on a fresh VM. |
| `register_remote_agent` | Finish bootstrapping a remote agent over SSH and register it with lair. |
| `terminate_agent` | Permanently terminate a child agent and delete its data + workspace dirs. |
| `forget_agent` | Remove an agent's registry row without touching any process or VM. |
| `restart_all_agents` | Stop and respawn every managed local agent (e.g. after a binary upgrade). |

## Child-agent spawn tools

Defined in [`lair/src/agent.rs`](../lair/src/agent.rs) `make_extra_tools()`. A child
agent only sees these if lair handed it a capability token at spawn time — i.e. only
*agent-spawned* children, not operator-spawned top-level children.

| Tool | Purpose |
|------|---------|
| `spawn_agent` | Spawn a new child agent owned by this agent. |
| `terminate_agent` | Terminate an agent this agent spawned (or any transitive descendant). |

## Conditional tools

A few tools appear only when a runtime condition is met:

| Tool | Condition |
|------|-----------|
| `web_search` | `BRAVE_API_KEY` is set in the environment (searches via Brave Search). |
| `spawn_agent` / `terminate_agent` (child) | `OCTO_AGENT_TOKEN` and `LAIR_INTERNAL_URL` are both set — granted only to agent-spawned children. |

MCP server tools are also merged in at runtime when configured; `tool_definitions_with_mcp()`
skips any MCP tool whose name collides with a built-in.
