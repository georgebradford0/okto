# Customization

Three operator knobs shape what lair (and the agents it spawns) does at boot and
on every turn, plus the Git integration most coding work relies on.

## `bootstrap.sh`

A bash script the container entrypoint runs **once at startup, before** the
management API binds. Use it for one-time setup that needs root inside the
container: extra `apt-get install`s, pre-cloning a shared repo into `/data`,
configuring `git`, warming caches.

It lives on the host at `~/.okto/bootstrap.sh` (mounted at `/data/bootstrap.sh`).
Drop it in and restart:

```sh
cp my-bootstrap.sh ~/.okto/bootstrap.sh
okto reload
```

Because every local agent **shares lair's container**, anything the script
installs into the shared filesystem (`apt-get install`, `npm i -g`,
`uv tool install`, …) lands on every agent's `PATH`. It runs on each container
start, so keep it **idempotent**. A non-zero exit aborts boot — if it does heavy
work, raise the health wait with `--ready-timeout` on
[`okto init`](getting-started.md#useful-okto-init-flags) / `okto reload`.

## `startup_prompt` (per child agent)

When you (or the model) create a child agent, you can pass an optional
**`startup_prompt`** — sent as the agent's first *user* message once it's
healthy, triggering a full agentic turn. Use it to hand the agent its opening
task: *"clone X, run the tests, summarize what's failing."*

Never put secrets in it — provider credentials propagate via env automatically.
There is no per-agent startup script; shared package installs belong in
[`bootstrap.sh`](#bootstrapsh).

## System prompt

lair's built-in system prompt is compiled into the binary, but you can **append**
free-form text to it without rebuilding — house style, deploy conventions,
project commands, anything the model should always see.

Set it at init:

```sh
okto init --system-prompt-append "Prefer pnpm over npm. Deploys go through ArgoCD."
okto init --system-prompt-append @./lair-prompt.md      # read from a file
```

Or change it later:

```sh
okto config set --system-prompt-append @./lair-prompt.md
okto config set --system-prompt-append ""               # clear it
```

It's stored as `system_prompt_append` in `~/.okto/config.json` and **re-read
every turn**, so edits take effect immediately — no reload. It only affects
lair's own prompt; child agents have their own context (a `CLAUDE.md` in a cloned
repo, or their assigned purpose).

## GitHub / GitLab

okto was built for managing coding projects on GitHub, so `gh`, `glab`, and `git`
are baked into the lair image and every agent. To use them, supply a token as an
env var:

```sh
okto init -e GH_TOKEN=ghp_xxx
# or later:
okto env set GH_TOKEN=ghp_xxx        # GITLAB_TOKEN works the same way
okto reload
```

The token is needed even if you also add a GitHub/GitLab MCP, because the `git`
CLI manages repos in agent workspaces. In general, prefer **a command-line client
+ an env var** over standing up an MCP — see [MCP servers](mcp.md).

## Building container images

Child agents run unprivileged, so the image ships [Buildah](https://buildah.io)
for daemonless OCI image builds (the agent invokes it from its bash tool). lair
builds rootful; each child uid gets a subordinate-uid range for rootless builds.
Where possible, prefer external CI (e.g. GitHub Actions) for image builds.
