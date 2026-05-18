# octo
`octo` is a mobile agent management system that runs a fleet of local and remote LLM agents. It was originally designed for coding but can be used to deploy LLM agents for any task.  It supports the Anthropic and any OpenAI-compatible API.  It uses the Noise Protocol to setup an encrypted connection between mobile and server without the need for DNS.

## Setup
To get up and running you'll need
- A Linux host with static IP with ports 22 and 8443 open
- LLM provider api key (you can also setup open source models but we'll defer explaining)
- iPhone (or you can build Android if you like)

Grab the CLI on your linux host with the helper script
```sh
curl -fsSL https://raw.githubusercontent.com/georgebradford0/octo/main/scripts/get-cli.sh | sh
```
then
```
octo init
```
It will prompt for:
- **Anthropic API key** — press Enter to skip.
- **OpenAI API key** — press Enter to skip. At least one of the two keys is required.
- **API URL** — Enter for the Anthropic default; otherwise the full chat-completions URL (e.g. `https://api.deepinfra.com/v1/openai/chat/completions`).
- **Model** — e.g. `claude-sonnet-4-6`.

`init` will then:

1. Persist credentials to `~/.octo/config.json`.
2. Install docker if not already available
3. Generate a Noise keypair and an Ed25519 SSH keypair (the SSH key is reserved for ops backchannels — e.g. SSHing into a remote host for tailing logs).
4. Write an env file (`~/.octo/lair-env`) — this is what `docker --env-file` ingests.
5. `docker pull` the lair image, then `docker run -d --name octo-lair -v ~/.octo:/data -p 8443:8443 …`.
6. Wait for the management API on `127.0.0.1:8000/health`, then print a QR code containing the host, port, and Noise pubkey.

Once the QR code prints to the terminal, download the mobile app at (TODO build ios for production and list here) or build code in `mobile/` to local device (iOS or Android).  Open the app, press icon and scan the QR.

If you are on iOS it will ask for push notification permissions.  These are generally for background tasks or monitors, but technically there is a dedicated tool for push notifications so you can always direct the model to call that tool for any scenario you want.  I have set up a small relay server to handle these push notifications.  It does not require sign up.  If you'd like to understand how it authenticates the device you can read [this](docs/relay-architecture.md).

To tear down everything except the config, run
```
octo destroy
```
## Github 
The project was originally built to manage coding-based projects on Github.  For this reason the `gh` command line client is installed by default on `lair` and all agents.  To use it you'll have to pass in GH_TOKEN as an environment variable, which can be done at init with
```
octo init -e GH_TOKEN=<token>
```
or after initialization with 
```
octo env set GH_TOKEN=<token>
octo reload
```
The Github (or Gitlab) MCP can always be added to `lair` instead. It will be propagated to child agents by default.

## MCP Support
MCP servers can be seeded at init time by passing an MCP JSON file:
```sh
octo init --mcp-config <path_to_mcp_json>
```
An example file is [here](.mcp.json). They can also be added at runtime with CLI and are hot-reloaded 
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
One thing to note.  MCPs by default are inherited by parent to spawned child.  This will probably change but I haven't decided on a design handle MCP inheritance in detail.  Currently the CLI can only update MCPs for local agents, this will also change soon.

## Docker-in-Docker
Building docker images from a chat is currently not available.  This will change in the future.  I currently use Github workflows to run docker builds.

## Local vs Remote Agents
Agents can be deployed and managed from the main chat or using the CLI. Local agents are deployed in the *same container* as `octo-lair` but with their own data dir.  This is so `octo-lair` does not have docker.sock access and is completely contained on the host.  Once an agent is deployed and ready to communicate it will be available in the mobile sidebar with a separate chat.

For remote agents an MCP for AWS/Azure/GCP is necessary (any provider is fine assuming they have an MCP, which they probably do).  Assuming the MCP exists, you can simply ask the main chat to create a remote agent on whatever instance type (eg for AWS a t3.micro) is preferred.  It will provision said instance with the appropiate `userdata` using builtin tools to continue setup once the instance comes online, connect through SSH and complete registration of the remote agent.  As per the local case, remote agents also run in a docker container without sock access and any agents added will be in the same container.  A list of builtin tools is [here](docs/builtin-tools.md).

