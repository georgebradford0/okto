# claudulhu

An agentic coding assistant. Rulyeh manages a fleet of per-repo coding assistant containers and exposes the client-facing interface. Clients connect via an encrypted Noise tunnel.

## Architecture

| Component | Role |
|---|---|
| **Rulyeh** | Master node — manages child containers, exposes the Noise WebSocket interface |
| **Child** | One per repo — handles the agentic coding loop for a single Git repository |

Both roles use the same image: `ghcr.io/georgebradford0/rulyeh:latest`

---

## Install the CLI

```sh
curl -fsSL https://raw.githubusercontent.com/georgebradford0/claudulhu/main/scripts/get-cli.sh | sh
```

---

## Setup

### 1. Open firewall ports

In your cloud provider's firewall / security group, allow **inbound TCP** on:

| Port | Used by |
|------|---------|
| 30900 | rulyeh Noise tunnel (mobile connects here) |
| 30100–30199 | Child container Noise tunnels |

### 2. Bootstrap rulyeh

```sh
claudulhu init --api-key sk-ant-... --gh-token ghp_...
```

This single command:
- Installs k3s if no cluster is reachable (Linux only)
- Creates the `claudulhu` namespace and RBAC
- Generates and stores the Noise keypair
- Deploys rulyeh and waits for it to be ready
- Prints the QR code to scan with the mobile app

Options:

| Flag | Default | Description |
|------|---------|-------------|
| `--api-key` | `$ANTHROPIC_API_KEY` | Anthropic API key |
| `--gh-token` | `$GH_TOKEN` | GitHub token (optional) |
| `--noise-port` | `30900` | NodePort for the Noise tunnel |

---

## Creating child containers

### Via chat

Once connected in the app, ask rulyeh:

> "Create a container for https://github.com/user/repo"

Rulyeh creates a Kubernetes Deployment, two PVCs (`/data` and `/workspace`), a ClusterIP Service, and a NodePort Service — NodePorts are assigned from **30100–30199**.

### Via CLI

```sh
claudulhu containers create --git-url https://github.com/user/repo
claudulhu containers list
claudulhu containers stop   <name>
claudulhu containers start  <name>
claudulhu containers delete <name>
```

---

## Operations

### Restart and update

Pull the latest image and restart all pods (rulyeh + all child containers):

```sh
claudulhu restart
```

### Logs

```sh
claudulhu logs            # all pods
claudulhu logs rulyeh     # rulyeh only
claudulhu logs -f rulyeh  # follow
```

---

## MCP tools

MCP servers extend what rulyeh and child containers can do. Both `npx` and `uvx` are pre-installed in the image. Add servers with the CLI — arguments for the command go after `--`:

```sh
# uvx-based server
claudulhu mcp add --name aws-ec2 --command uvx \
  --env AWS_ACCESS_KEY_ID=... --env AWS_SECRET_ACCESS_KEY=... --env AWS_REGION=us-east-1 \
  -- awslabs.amazon-ec2-mcp-server

# uv run (for packages without a script entry point)
claudulhu mcp add --name prime-intellect --command uv \
  --env PRIME_API_KEY=... \
  -- run --with prime-mcp-server python -m prime_mcp.mcp

# Add to a specific child container (default is rulyeh)
claudulhu mcp add --container rulyeh-myrepo --name linear --command npx \
  --env LINEAR_API_KEY=lin_api_... \
  -- -y @linear/mcp-server

claudulhu mcp list
claudulhu mcp remove --name github
```

`mcp add` waits for the server to connect and reports the result. On failure the entry is automatically removed. The server config is stored in `/data/mcp.json` inside the container and hot-reloaded within a few seconds — you can also ask rulyeh to add MCP servers directly from chat.

---

## Environment variables

### Rulyeh

| Variable | Required | Description |
|---|---|---|
| `ANTHROPIC_API_KEY` | Yes | Anthropic API key |
| `GH_TOKEN` | No* | GitHub token — passed to every child |
| `PUBLIC_HOST` | No | Public IP/hostname for the QR code. Auto-detected if not set. |
| `NOISE_PORT` | No | Noise endpoint port (default: `9000`) |

*Required in practice for any GitHub repo work.

### Child containers

| Variable | Required | Description |
|---|---|---|
| `GIT_URL` | Yes | Repository to clone |
| `ANTHROPIC_API_KEY` | Yes | Anthropic API key |
| `GH_TOKEN` | No | GitHub token (required for private repos and PR creation) |
| `PUBLIC_HOST` | No | Public IP/hostname for the QR code. Auto-detected if not set. |
| `NOISE_PORT` | No | Noise endpoint port (default: `9000`) |
| `GIT_USER_NAME` / `GIT_USER_EMAIL` | No | Git commit author identity |
| `STARTUP_SCRIPT` | No | Shell script run before the server starts |
| `STARTUP_PROMPT` | No | Initial prompt sent to the agentic loop on startup |

---

## Git authentication

**HTTPS** (`https://github.com/user/repo`)
- Authenticated via `GH_TOKEN` — used for both clone and push

**SSH** (`git@github.com:user/repo.git`)
- Authenticated via SSH key mounted at `/root/.ssh/id_rsa`
- `GH_TOKEN` is ignored when using SSH URLs

---

## PR/MR creation

The agentic loop has a `create_pull_request` tool that opens a GitHub PR or GitLab MR after pushing a branch. Requires `GH_TOKEN` with `repo` (GitHub) or `api` (GitLab) scope. Not available when using SSH authentication.
