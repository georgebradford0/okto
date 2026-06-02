# Managing lair

Day-to-day operation of the running lair container: credentials, env vars,
restarts, logs, and image updates.

## Credentials & model (`okto config`)

`~/.okto/config.json` holds your provider credentials and model settings. **Lair
re-reads it on every turn**, so changes apply live — no restart needed.

```sh
okto config show        # values with secrets masked
```

Update individual fields:

```sh
okto config set --model claude-sonnet-4-6
okto config set --api-url https://api.deepinfra.com/v1/openai/chat/completions
okto config set --anthropic-api-key sk-ant-...
okto config set --openai-api-key sk-...
```

| Flag | Updates |
|------|---------|
| `--model` | Model name |
| `--api-url` | API endpoint URL |
| `--anthropic-api-key` | Anthropic key |
| `--openai-api-key` | OpenAI-compatible key |
| `--system-prompt-append <TEXT or @PATH>` | Replaces the system-prompt append; `""` clears it. See [Customization](customization.md#system-prompt). |
| `--cost-input1m <USD>` | Input-token price (USD / 1M tokens) for cost estimates on OpenAI-compatible backends. Negative clears it. |
| `--cost-output1m <USD>` | Output-token price (USD / 1M tokens). Negative clears it. |

At least one of the Anthropic / OpenAI keys must remain set.

## Environment variables (`okto env`)

`~/.okto/lair-env` holds extra `KEY=VALUE` env vars passed to the lair container
(`docker --env-file`). They're inherited by every child agent lair spawns. Use
them for things like `GH_TOKEN`. **Changing env vars auto-restarts lair** (the
file is only read at container start).

```sh
okto env show                       # operator vars, values masked
okto env set GH_TOKEN=ghp_xxx       # upsert (repeatable)
okto env set FOO=bar BAZ=qux
okto env unset GH_TOKEN             # remove (repeatable)
```

!!! note "Reserved keys"
    Internal keys (`NOISE_PORT`, `PUBLIC_PORT`, `OKTO_HOME`, `OKTO_DATA_DIR`,
    `OKTO_AGENTS_DIR`, `OKTO_SKIP_SHELL_ENV`, `OKTO_LAIR_BINARY`, `HOME`) are
    managed for you and can't be set or unset.

A few special vars you *can* set with `okto env`:

| Var | Effect |
|-----|--------|
| `PUBLIC_HOST` | Host the QR code advertises (overrides auto-detected IP). |
| `OKTO_DEV=1` | Use loopback (`127.0.0.1`) in the QR code when `PUBLIC_HOST` is unset. |
| `OKTO_RELAY_URL=` | Empty value disables [push notifications](notifications.md). |

## Restarting (`okto reload`)

Restart lair to pick up env/config changes, optionally upserting env vars in the
same step:

```sh
okto reload
okto reload -e GH_TOKEN=ghp_new          # upsert env, then restart
okto reload --agents lair-myrepo         # also restart only these agents
okto reload --ready-timeout 300          # wait longer for health
```

Without `--agents`, every managed agent is restarted along with lair.

## Logs (`okto logs`)

```sh
okto logs                 # lair's logs (docker logs)
okto logs -f              # follow
okto logs lair-myrepo     # a child agent's agent.log (last 1 MB)
okto logs lair-myrepo -f
```

## Updating the lair image

`okto update` upgrades the CLI; to upgrade the **runtime**, pull a new lair
image and restart the container:

```sh
okto lair update                 # pull latest, restart, respawn running agents
okto lair update --image ghcr.io/georgebradford0/lair:0.21.4
okto lair version                # version of the running lair binary
```

`okto lair update` preserves which local agents were running and respawns them
after the restart.

## Where things live on the host

Everything is under `~/.okto` (bind-mounted to `/data` in the container):

| Path | What |
|------|------|
| `~/.okto/config.json` | Credentials + model (live-reloaded) |
| `~/.okto/lair-env` | Operator env vars (`docker --env-file`) |
| `~/.okto/lair-launch.json` | Ports + image, for `okto reload` |
| `~/.okto/lair/` | Lair's data: Noise key, `mcp.json`, `agents.json`, tasks, mgmt token |
| `~/.okto/agents/<name>/` | Per-agent `data/`, `workspace/`, `.ssh/`, and `agent.log` |
| `~/.okto/.ssh/` | The container's shared SSH keypair |

The CLI reaches lair over the **loopback-only** management API
(`127.0.0.1:8000`); state-changing calls are authenticated with a token in
`~/.okto/lair/.mgmt-token`. None of this is exposed to the network.
