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

To tear down everything except the config, run
```
okto destroy
```
## Github 
The project was originally built to manage coding-based projects on Github.  For this reason the `gh` command line client is installed by default on `lair` and all agents.  To use it you'll have to pass in GH_TOKEN as an environment variable, which can be done at init with
```
okto init -e GH_TOKEN=<token>
```
or after initialization with 
```
okto env set GH_TOKEN=<token>
okto reload
```
The Github (or Gitlab) MCP can always be added to `lair` instead. It will be propagated to child agents by default.  The caveat for this is that there is no dedicated tools list in the LLM prompt for Github, so usually the model has to be directed by the user to use `gh` but significantly shortens the prompt prefix length.  The system prompt references `gh` and explains that the model has access to it.  If you decide to go with the MCP, the inline `GH_TOKEN` to `init` is not needed and the env var should added in the MCP setup.

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
One thing to note.  MCPs by default are inherited by parent to spawned child.  This will probably change but I haven't decided on a design to handle MCP inheritance in detail.  Currently the CLI can only update MCPs for local agents, this will probably change soon.

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
Many GPU providers do not use VM-based systems for managing compute but instead use containers.  This makes it very difficult to setup a `lair` container on the GPU instance.  For this reason, agents should typically not be created in instances on typical GPU cloud providers.  Each GPU platform will likely have an MCP which can be used for provisioning compute (with API key) but not connecting to the instance itself.  To allow agents to connect just add SSH pubkey created in `lair` to your preferred GPU platform.  The pubkey can be found by using CLI command
```
okto ssh pubkey
```
SSH keys are generated on a per-container basis, so be aware that any remote agents created will not have the same keys as the main `lair` container. 

This only refers to smaller GPU cloud services.  All the hyperscalers use VMs and include `cloud-init`, so deploying remote agents from `lair` on remote instances follows the normal process per above.  

