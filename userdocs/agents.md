# Agents

lair is the parent agent. It can supervise **child agents** — each with its own
chat in the mobile sidebar, its own workspace, and (optionally) its own cloned
repo. Children's traffic is always **proxied through lair**, so they never get a
public network surface.

## Creating agents

Agents are **created from the chat**, not the CLI. Ask lair (in the mobile app)
to create one — for example *"create an agent for my repo github.com/me/app and
run the tests"*. Lair uses its built-in tools to spawn the agent, clone the repo
into the agent's workspace, and (optionally) kick off a startup task. See
[`startup_prompt`](customization.md#startup_prompt-per-child-agent).

The CLI manages the **lifecycle** of agents that already exist.

## Managing agents (`okto agents`)

```sh
okto agents list                 # all agents: id, name, kind, status, port, pid, host
okto agents start <id|name>      # start a stopped agent
okto agents stop <id|name>       # stop a running agent
okto agents delete <id|name>     # remove the agent + its data/workspace dirs
okto agents delete <id|name> -y  # skip the confirmation prompt
```

`okto agents list` reads `~/.okto/lair/agents.json` directly, so it works even
when lair isn't running. `start`/`stop`/`delete` go through lair's management
API.

Each agent has a free-form **name** (the label you see in the app, which may
contain spaces) and a route-safe **id** (a lowercase, hyphenated slug derived
from the name — also the on-disk directory name under `~/.okto/agents/<id>/`).
`okto agents list` shows both. The `start`/`stop`/`delete` commands accept
**either** the id or an unambiguous name.

!!! danger "delete is irreversible"
    `okto agents delete` wipes the agent's `data/` and `workspace/` directories.
    Anything not pushed/committed elsewhere is lost.

## Local vs remote agents

- **Local agents** run in the **same container** as lair, each with its own data
  directory and workspace. They appear in the mobile sidebar with a separate
  chat. Because they share lair's container, package/tool installs belong in
  [`bootstrap.sh`](customization.md#bootstrapsh), not per-agent setup.

- **Remote agents** run on a separate VM you provision. From the chat, ask lair
  to create a remote agent on an instance type of your choosing. Lair provisions
  the instance with the right `cloud-init` user-data, connects over SSH (using
  the container's [SSH key](#ssh-identity)), finishes setup, and registers the
  remote agent — after which it appears in the sidebar like any other. The cloud
  provider must use **VM-based instances with `cloud-init`** so SSH bootstrap
  works. This typically needs a cloud-provider MCP (AWS/Azure/GCP) configured so
  lair can call the provisioning API — see [MCP servers](mcp.md).

Remote agents also run in an unprivileged Docker container; additional agents
created there share that container.

## GPU instances (RunPod, Lambda, Prime Intellect, …)

Many GPU providers hand you a **container**, not a VM, so you can't stand up a
full lair container there with `cloud-init`. Instead:

1. Provision the GPU box (often via the provider's MCP + API key).
2. Register lair's SSH public key on the provider so agents can connect:

```sh
okto ssh pubkey
```

Add the printed key to the GPU platform's authorized keys / SSH settings.

!!! warning "Keys are per-container"
    SSH keys are generated per lair container. A **remote** agent on its own VM
    generates *its own* key — it won't share the main lair container's key. Use
    `okto ssh pubkey` on the relevant host.

The hyperscalers (AWS/Azure/GCP) all use VMs with `cloud-init`, so they follow
the normal remote-agent flow above rather than this GPU path.

## SSH identity

Every lair container holds one Ed25519 SSH keypair, shared by lair and all its
local agents, used for outbound SSH (`git`, `gh`, remote VMs, GPU pods).
Register the public key once per external service:

```sh
okto ssh pubkey
```

(For the transport-layer Noise keypair and the full key story, see the
developer docs.)

## Worktrees

A repo-bound agent can host **git worktrees** of its workspace — each on its own
branch with its own chat. They show up indented under the agent in the mobile
sidebar. Worktrees are created and managed from the app, not the CLI.
