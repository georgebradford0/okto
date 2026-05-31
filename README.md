# okto
`okto` is a mobile agent management system that runs a fleet of local and remote LLM agents. Using the Noise Protocol it allows users to setup an encrypted connection between iOS/Android and chat server without the need for DNS.  Works with Anthropic and/or OpenAI-compatible APIs.

This code is experimental and will change frequently between version updates.

## Setup
To get up and running you'll need
- A Linux host with static IP with ports 22 and 8443 open
- LLM provider API key (you can also setup open source models but we'll defer explaining)  // TODO create section explaining self hosted requirements and link
- iPhone (or you can build Android if you like)

Grab the CLI on your linux host with the helper script
```sh
curl -fsSL https://raw.githubusercontent.com/georgebradford0/okto/main/scripts/get-cli.sh | sh
```
then
```
okto init
```
It will prompt for:
- **Anthropic API key** — press Enter to skip.
- **OpenAI API key** — press Enter to skip. At least one of the two keys is required.
- **API URL** — Enter for the Anthropic default; otherwise the full chat-completions URL (e.g. `https://api.deepinfra.com/v1/openai/chat/completions`).
- **Model** — Enter for default of `claude-sonnet-4-6`.

`init` will then:

1. Persist credentials to `~/.okto/config.json`.
2. Install docker if not already available
3. Generate a Noise keypair and an Ed25519 SSH keypair (the SSH key is reserved for ops backchannels — e.g. SSHing into a remote host for tailing logs).
4. Write an env file (`~/.okto/lair-env`) — this is what `docker --env-file` ingests.
5. `docker pull` the lair image, then `docker run -d --name lair -v ~/.okto:/data -p 8443:8443 …`.
6. Wait for the management API on `127.0.0.1:8000/health`, then print a QR code containing the host, port, and Noise pubkey.

Once the QR code prints to the terminal, download the mobile app at (TODO build ios for production and list here) or build code in `mobile/` to local device (iOS or Android).  Open the app, press icon and scan the QR.

If you are on iOS it will ask for push notification permissions.  These are generally for background tasks or monitors, but technically there is a dedicated tool for push notifications so you can always direct the model to call that tool for any scenario you want.  I have set up a small relay server to handle these push notifications.  It does not require sign up.  If you'd like to understand how it authenticates the device you can read [this](docs/relay-architecture.md).

### Push notifications (opt-out)
Push notifications are **on by default** — lair points at the relay server above, and the mobile app registers its APNs device token with it so background tools (`send_notification`, `ask_question`) can wake you when an agent has something to report.

If you'd rather not use them, opt out at init time:
```sh
okto init --disable-push
```
This persists `OKTO_RELAY_URL=` (an explicit empty value) into `~/.okto/lair-env`, which (a) drops the `send_notification` and `ask_question` tools from the LLM's tool list in both lair and child agents — so the model never offers to push at all — and (b) makes lair's `/info` advertise an empty relay URL, which the mobile client treats as a signal to skip APNs registration entirely.

To turn push back on later without re-running `init`:
```sh
okto env unset OKTO_RELAY_URL
okto reload
```

To tear down everything except the config, run
```
okto destroy
```
## Github 
The project was originally built to manage coding-based projects on Github.  For this reason the `gh`/`glab` command line clients are installed by default on `lair` and all agents.  To use it you'll have to pass in `GH_TOKEN`/`GITLAB_TOKEN` as an environment variable, which can be done at init with
```
okto init -e GH_TOKEN=<token>
```
or after initialization with 
```
okto env set GH_TOKEN=<token>
okto reload
```
The Github/Gitlab MCPs can be added but unfortunately it the token is still required, since `git` command line client is used for managing repo in agent workspaces.  The reason for this design choice is part of a larger problem regarding MCP isolation from bash calls. It might be possible to get bash scripts to interact with MCP servers but it would be extremely brittle, especially since many time the models are generating the bash scripts and they are trained to write bash scripts for command line clients and not fetching from some MCP server.  Given this it's better to just use the command line client and append reference to the system prompt for `lair`.   

In general I'm on the fence about MCPs.  It's hard to find a situation where just referencing the availabilty of a command line client along with adding env vars to container isn't better than setting up an MCP for it instead.  

## Startup Scripts & System Prompt

There are three operator-facing knobs for shaping what `lair` (and the agents it spawns) does at boot or on every turn:

### `bootstrap.sh` (container startup)
A bash script the container entrypoint runs *before* the management API binds. Use it for one-time setup that needs root inside the container — installing extra apt packages, pre-cloning a shared repo into `/data`, configuring `git`, baking caches, etc.

It lives at `~/.okto/bootstrap.sh` on the host (mounted at `/data/bootstrap.sh` in the container). Drop the file in place and restart:
```sh
cp my-bootstrap.sh ~/.okto/bootstrap.sh
okto reload
```
Because every local agent shares lair's container, anything the script installs into the shared filesystem (`apt-get install`, `npm i -g`, `uv tool install`, …) lands on every agent's `PATH`. So it runs **once**, on the container's entrypoint process — lair, or a standalone remote agent — and locally-spawned children inherit the result rather than re-running it. The script runs every time the container starts; keep it idempotent. A non-zero exit aborts boot.

### `startup_prompt` (per child agent)
When you (or the model) creates a child agent — either via the mobile chat's `create_agent` tool or `POST /agents` on lair's management API — you can pass an optional **`startup_prompt`**: sent as the child's first *user* message once it's healthy, triggering a full agentic turn. Use it to hand the agent its initial task ("clone X, run the tests, summarize what's failing").

Never put secrets in it — provider credentials are propagated through env automatically. There's no per-agent startup script: agents share lair's container, so package/tool installs belong in `bootstrap.sh` above.

### `--system-prompt-append` (lair's system prompt)
Lair's built-in system prompt is hardcoded into the binary, but you can append free-form text to it without rebuilding. Use this for site-specific guidance the model should always see — house style, deployment conventions, project-specific commands, etc.

Set it at init time:
```sh
okto init --system-prompt-append "Prefer pnpm over npm. Production deploys go through ArgoCD."
# or load from a file:
okto init --system-prompt-append @./lair-prompt.md
```
Or update it later:
```sh
okto config set --system-prompt-append @./lair-prompt.md
okto config set --system-prompt-append ""   # clears the override
```

The value is stored as `system_prompt_append` in `~/.okto/config.json` and re-read on every turn, so edits take effect immediately — no `okto reload` required. The append only affects lair's own prompt; child agents have their own (a CLAUDE.md in the cloned repo for repo-bound agents, or `AGENT_PURPOSE` for general-purpose ones).

## MCP Support
MCP servers can be seeded at init time by passing an MCP JSON file:
```sh
okto init --mcp-config <path_to_mcp_json>
```
An example file is [here](.mcp.json). They can also be added at runtime with CLI and are hot-reloaded 
```sh
# uvx-based server
okto mcp add --name aws-ec2 --command uvx \
  --env AWS_ACCESS_KEY_ID=... --env AWS_SECRET_ACCESS_KEY=... --env AWS_REGION=us-east-1 \
  -- awslabs.amazon-ec2-mcp-server

# Add to a specific child agent (default is lair)
okto mcp add --agent lair-myrepo --name linear --command npx \
  --env LINEAR_API_KEY=lin_api_... \
  -- -y @linear/mcp-server

okto mcp list
okto mcp remove --name github
```
One thing to note.  MCPs by default are inherited by parents to spawned children.  This will probably change but I haven't decided on a design to handle MCP inheritance in detail.  Currently the CLI can only update MCPs for local agents, this will probably change soon.

## Building Docker Images
Since child agents do not have root access the image includes [Buildah](https://buildah.io) for daemonless Docker/OCI image builds. Agents invoke it from their bash tool:

Lair (running as root) builds rootful; each child agent uid (10100..10199) gets its own subordinate-uid range so it can build rootless. The image is configured with the `vfs` storage driver — slow and disk-heavy, but works without `/dev/fuse` or extra `docker run` flags. Per-agent build storage lives under `$HOME/.local/share/containers/storage` (= `~/.okto/agents/<name>/.local/share/...` on the host).

Though `lair` does have root access within the container, any builds from there should also use `buildah`.  In general, if possible docker builds should be handled by external CI setup like Github Actions.  

## Noise/SSH Keys
Every lair container holds two independent keypairs, each with a distinct purpose:

- **Noise keypair (X25519)** — the transport identity. Mobile↔lair and lair↔remote-agent traffic is wrapped in a `Noise_XX_25519_ChaChaPoly_SHA256` tunnel; the pubkey is what the QR code embeds, and what mobile pins as the *expected* responder static. This is how DNS / TLS PKI is avoided entirely.
- **SSH keypair (Ed25519)** — the operator backchannel. Lair uses it to SSH into freshly-provisioned remote VMs during bootstrap, and every local child agent's `~/.ssh/` is seeded from the same keypair so the whole container shares one external identity for `gh`, `git`, GPU-pod SSH, etc. Register it on external services once with `okto ssh pubkey`.

Both keys are container-scoped — a remote lair on another host generates its own pair at startup; only pubkeys ever cross between containers. For the full picture (handshake mechanics, bootstrap flow, rotation), see [docs/keypairs.md](docs/keypairs.md).

## Local vs Remote Agents
Agents can be deployed and managed from the main chat or using the CLI. Local agents are deployed in the *same container* as `lair` but with their own data directory.  Once an agent is deployed and ready to communicate it will be available in the mobile sidebar with a separate chat.

For remote agents an MCP for AWS/Azure/GCP is necessary (any provider is fine assuming they have an MCP, which they probably do).  Assuming the MCP exists, you can simply ask the main chat to create a remote agent on whatever instance type (eg for AWS a t3.micro) is preferred.  It will provision said instance with the appropiate `userdata` using builtin tools to continue setup once the instance comes online, connect through SSH and complete registration of the remote agent.  Whatever cloud provider must user VM-based instances with typical `cloud-init` setup so SSH can be achieved.  The next section goes over dealing with typical GPU provider setups.  

As per the local case, remote agents also run in an unpriveleged docker container and any agents added will be in the same container. 

## GPU Instances (RunPod, Lambda, Prime Intellect etc)
Many GPU providers do not use VM-based systems for managing compute but instead use containers.  This makes it very difficult to setup a `lair` container on the GPU instance.  Each GPU platform will likely have an MCP which can be used for provisioning compute (with API key) but not connecting to the instance itself.  To allow agents to connect just add SSH pubkey created in `lair` to your preferred GPU platform.  The pubkey can be found by using CLI command
```
okto ssh pubkey
```
SSH keys are generated on a per-container basis, so be aware that any remote agents created will not have the same keys as the main `lair` container. 

This only refers to smaller GPU cloud services.  All the hyperscalers use VMs and include `cloud-init`, so deploying remote agents from `lair` on remote instances follows the normal process per above.  

