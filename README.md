# claudulhu

An agentic coding assistant that runs a Rust server, exposing a WebSocket API for mobile and other clients to interact with a git repository via Claude.

## Docker

The server is available as a multi-platform Docker image (`linux/amd64`, `linux/arm64`).

Pull the latest image:

```sh
docker pull ghcr.io/georgebradford0/claudulhu-server:latest
```

```sh
docker run -d \
  --name claudulhu \
  -p 9000:9000 \
  -v claudulhu-noise-key:/etc/claudulhu \
  -v claudulhu-data:/data \
  -e GIT_URL=https://github.com/user/repo \
  -e GH_TOKEN=ghp_... \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  ghcr.io/georgebradford0/claudulhu-server:latest
```

On startup the container prints a QR code. Scan it with the mobile app to connect — the app establishes an encrypted Noise Protocol tunnel (port 9000) and routes all traffic through it. No TLS certificate required.

The named volume (`claudulhu-noise-key`) persists the server's Curve25519 keypair across container restarts, so the QR code remains valid. Without it, the key regenerates on every restart and the app must re-scan.

The named volume (`claudulhu-data`) persists conversation history, config, and worktrees at `/data` inside the container. Without it, history survives container restarts (`docker stop`/`start`) but is lost when the container is removed or replaced with a new image.

### Environment variables

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

Each container is independent. Set `NOISE_PORT` to the host port you want to expose — the server binds on that port inside the container and encodes it in the QR code:

```sh
docker run -d \
  --name claudulhu-repo-a \
  -p 9000:9000 \
  -v claudulhu-key-a:/etc/claudulhu \
  -v claudulhu-data-a:/data \
  -e NOISE_PORT=9000 \
  -e PUBLIC_HOST=1.2.3.4 \
  -e GIT_URL=https://github.com/user/repo-a \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  ghcr.io/georgebradford0/claudulhu-server:latest

docker run -d \
  --name claudulhu-repo-b \
  -p 9001:9001 \
  -v claudulhu-key-b:/etc/claudulhu \
  -v claudulhu-data-b:/data \
  -e NOISE_PORT=9001 \
  -e PUBLIC_HOST=1.2.3.4 \
  -e GIT_URL=https://github.com/user/repo-b \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  ghcr.io/georgebradford0/claudulhu-server:latest
```

Ports 9000–9099 are available on the default AWS deployment. Each container needs its own named volumes so keypairs (and therefore QR codes) and conversation history persist independently across restarts.

### AWS quick deploy

Create a new Ubuntu EC2 instance with the container running on startup:

```sh
SG=$(aws ec2 create-security-group --group-name claudulhu-sg --description "claudulhu" --query 'GroupId' --output text) && \
aws ec2 authorize-security-group-ingress --group-id $SG --protocol tcp --port 9000 --cidr 0.0.0.0/0 && \
aws ec2 run-instances \
  --image-id resolve:ssm:/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id \
  --instance-type t3.micro \
  --key-name <your-key-pair> \
  --security-group-ids $SG \
  --user-data '#!/bin/bash
apt-get update -y
apt-get install -y docker.io
systemctl enable --now docker
docker run -d --name claudulhu --restart unless-stopped \
  -p 9000:9000 \
  -v claudulhu-noise-key:/etc/claudulhu \
  -v claudulhu-data:/data \
  -e GIT_URL=https://github.com/user/repo \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  ghcr.io/georgebradford0/claudulhu-server:latest' \
  --count 1 \
  --region us-east-1
```

Once the instance is running, get its public IP and check the logs for the QR code:

```sh
INSTANCE_ID=<instance-id>
aws ec2 describe-instances --instance-ids $INSTANCE_ID \
  --query 'Reservations[0].Instances[0].PublicIpAddress' --output text
ssh -i ~/.ssh/<your-key-pair>.pem ubuntu@<public-ip> "docker logs claudulhu"
```

### PR/MR creation

The agentic loop has a `create_pull_request` tool that opens a GitHub PR or GitLab MR after pushing a branch. Requires `GH_TOKEN` with `repo` (GitHub) or `api` (GitLab) scope. Not available when using SSH authentication.
