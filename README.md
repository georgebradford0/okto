# claudulhu

An agentic coding assistant that runs a Rust server, exposing a WebSocket API for mobile and other clients to interact with a git repository via Claude.

## Docker

The server is available as a multi-platform Docker image (`linux/amd64`, `linux/arm64`):

```sh
docker run -p 2222:2222 \
  -e GIT_URL=https://github.com/user/repo \
  -e GIT_TOKEN=ghp_... \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  ghcr.io/georgebradford0/claudulhu-server:latest
```

On startup the container prints a QR code. Scan it with the mobile app to connect — the app establishes an SSH tunnel (port 2222) and routes all traffic through it. No TLS certificate required.

### Environment variables

| Variable | Required | Description |
|---|---|---|
| `PUBLIC_HOST` | No | Public IP or hostname of the server. Auto-detected via `api.ipify.org` if not set. |
| `GIT_URL` | Yes | URL of the repository to clone |
| `ANTHROPIC_API_KEY` | Yes | Anthropic API key |
| `GIT_TOKEN` | No | GitHub/GitLab personal access token (required for pushing to private repos or creating PRs) |
| `GIT_USER_NAME` | No | Git commit author name (default: `claudulhu`) |
| `GIT_USER_EMAIL` | No | Git commit author email (default: `claudulhu@localhost`) |
| `SSH_PORT` | No | Port for the SSH tunnel endpoint (default: `2222`) |

### Git URL schemes

Two URL schemes are supported:

**HTTPS** (`https://github.com/user/repo`)
- Clone and push authenticated via `GIT_TOKEN`
- Token is embedded in the clone URL and set as a credential helper for subsequent pushes

**SSH** (`git@github.com:user/repo.git`)
- Clone and push authenticated via SSH key
- Mount your key at `/root/.ssh/id_rsa`: `-v ~/.ssh/id_rsa:/root/.ssh/id_rsa:ro`
- `GIT_TOKEN` is ignored when using SSH URLs — if both are provided, the token is silently unused

### Multiple repos

Each container is independent. Run one per repo on different SSH ports:

```sh
docker run -d -p 2222:2222 -e PUBLIC_HOST=1.2.3.4 -e SSH_PORT=22 -e GIT_URL=https://github.com/user/repo-a ...
docker run -d -p 2223:2223 -e PUBLIC_HOST=1.2.3.4 -e SSH_PORT=2223 -e GIT_URL=https://github.com/user/repo-b ...
```

### PR/MR creation

The agentic loop has a `create_pull_request` tool that opens a GitHub PR or GitLab MR after pushing a branch. Requires `GIT_TOKEN` with `repo` (GitHub) or `api` (GitLab) scope. Not available when using SSH authentication.
