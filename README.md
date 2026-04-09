# claudulhu

An agentic coding assistant. Rulyeh (the master node) manages a fleet of per-repo coding assistant containers and exposes the client-facing interface. Clients connect via an encrypted Noise tunnel.

## Architecture

| Component | Image | Role |
|---|---|---|
| **Rulyeh** | `ghcr.io/georgebradford0/claudulhu-rulyeh` | Master node — manages child containers, exposes the Noise WebSocket interface |
| **claudulhu-server** | `ghcr.io/georgebradford0/claudulhu-server` | Child container — one per repo, handles the agentic coding loop |

## Docker

### Rulyeh (master node)

Rulyeh is the entry point. It needs access to the Docker socket to spin up and manage child containers.

Pull the image:

```sh
docker pull ghcr.io/georgebradford0/claudulhu-rulyeh:latest
```

Run it:

```sh
docker run -d \
  --name claudulhu-rulyeh \
  -p 9000:9000 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v rulyeh-data:/data \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -e GH_TOKEN=ghp_... \
  ghcr.io/georgebradford0/claudulhu-rulyeh:latest
```

On startup the container prints a QR code. Scan it with the mobile app to connect — the app establishes an encrypted Noise Protocol tunnel (port 9000) and routes all traffic through it.

Once connected, ask Rulyeh to create a child container for a repository:

> Create a container for https://github.com/user/repo

Rulyeh will start a `claudulhu-server` container on the `claudulhu-net` bridge network. The child exposes its own Noise port and QR code for direct connection.

The named volume (`rulyeh-data`) persists the Noise keypair and session history across restarts. Without it, the keypair regenerates on every restart and the app must re-scan.

### Environment variables (Rulyeh)

| Variable | Required | Description |
|---|---|---|
| `ANTHROPIC_API_KEY` | Yes | Anthropic API key |
| `GH_TOKEN` | Yes* | GitHub token — passed to every child container |
| `PUBLIC_HOST` | No | Public IP or hostname of the server. Auto-detected via `api.ipify.org` if not set. |
| `NOISE_PORT` | No | Port for the Noise Protocol endpoint (default: `9000`) |

*`GH_TOKEN` is technically optional but Rulyeh will refuse to create child containers without it.

### Child containers (claudulhu-server)

Child containers are created and managed by Rulyeh. They can also be run standalone:

```sh
docker run -d \
  --name claudulhu \
  -p 9001:9001 \
  -v claudulhu-data:/data \
  -e GIT_URL=https://github.com/user/repo \
  -e GH_TOKEN=ghp_... \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -e NOISE_PORT=9001 \
  ghcr.io/georgebradford0/claudulhu-server:latest
```

### Environment variables (claudulhu-server)

| Variable | Required | Description |
|---|---|---|
| `GIT_URL` | Yes | URL of the repository to clone |
| `ANTHROPIC_API_KEY` | Yes | Anthropic API key |
| `PUBLIC_HOST` | No | Public IP or hostname of the server. Auto-detected via `api.ipify.org` if not set. |
| `GH_TOKEN` | No | GitHub/GitLab personal access token (required for pushing to private repos or creating PRs) |
| `GIT_USER_NAME` | No | Git commit author name (default: `claudulhu`) |
| `GIT_USER_EMAIL` | No | Git commit author email (default: `claudulhu@localhost`) |
| `NOISE_PORT` | No | Port for the Noise Protocol endpoint (default: `9000`) |

### Git URL schemes

Two URL schemes are supported:

**HTTPS** (`https://github.com/user/repo`)
- Clone and push authenticated via `GH_TOKEN`
- Token is embedded in the clone URL and set as a credential helper for subsequent pushes

**SSH** (`git@github.com:user/repo.git`)
- Clone and push authenticated via SSH key
- Mount your key at `/root/.ssh/id_rsa`: `-v ~/.ssh/id_rsa:/root/.ssh/id_rsa:ro`
- `GH_TOKEN` is ignored when using SSH URLs — if both are provided, the token is silently unused

### Multiple repos

Each child container is independent. When creating containers through Rulyeh, it allocates ports from the range 9100–9199 automatically. For standalone use, set `NOISE_PORT` to the host port you want to expose:

```sh
docker run -d \
  --name claudulhu-repo-a \
  -p 9100:9100 \
  -v claudulhu-data-a:/data \
  -e NOISE_PORT=9100 \
  -e PUBLIC_HOST=1.2.3.4 \
  -e GIT_URL=https://github.com/user/repo-a \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  ghcr.io/georgebradford0/claudulhu-server:latest

docker run -d \
  --name claudulhu-repo-b \
  -p 9101:9101 \
  -v claudulhu-data-b:/data \
  -e NOISE_PORT=9101 \
  -e PUBLIC_HOST=1.2.3.4 \
  -e GIT_URL=https://github.com/user/repo-b \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  ghcr.io/georgebradford0/claudulhu-server:latest
```

Each container needs its own named volume so keypairs and conversation history persist independently.

### AWS quick deploy

Create a new Ubuntu EC2 instance with Rulyeh running on startup:

```sh
SG=$(aws ec2 create-security-group --group-name claudulhu-sg --description "claudulhu" --query 'GroupId' --output text) && \
aws ec2 authorize-security-group-ingress --group-id $SG --protocol tcp --port 9000-9199 --cidr 0.0.0.0/0 && \
aws ec2 run-instances \
  --image-id resolve:ssm:/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id \
  --instance-type t3.micro \
  --key-name <your-key-pair> \
  --security-group-ids $SG \
  --user-data '#!/bin/bash
apt-get update -y
apt-get install -y docker.io
systemctl enable --now docker
docker run -d --name claudulhu-rulyeh --restart unless-stopped \
  -p 9000-9199:9000-9199 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v rulyeh-data:/data \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -e GH_TOKEN=ghp_... \
  ghcr.io/georgebradford0/claudulhu-rulyeh:latest' \
  --count 1 \
  --region us-east-1
```

Once the instance is running, get its public IP and check the logs for the QR code:

```sh
INSTANCE_ID=<instance-id>
aws ec2 describe-instances --instance-ids $INSTANCE_ID \
  --query 'Reservations[0].Instances[0].PublicIpAddress' --output text
ssh -i ~/.ssh/<your-key-pair>.pem ubuntu@<public-ip> "docker logs claudulhu-rulyeh"
```

### PR/MR creation

The agentic loop has a `create_pull_request` tool that opens a GitHub PR or GitLab MR after pushing a branch. Requires `GH_TOKEN` with `repo` (GitHub) or `api` (GitLab) scope. Not available when using SSH authentication.
