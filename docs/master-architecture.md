# claudulhu-maitred: Architecture

## Overview

`claudulhu-maitred` is a Docker container that acts as the single entry point for the mobile app. It runs a full AI agentic loop (same as `claudulhu-server`) with Docker socket access, so it can manage child repo containers via bash. The mobile app scans one QR code (master's), chats with master for control tasks, and taps into individual child containers for code work.

## Components

### `master/` (new Rust crate)

- Identical Noise_XX proxy and WebSocket handler to `server/`
- System prompt: Docker control node with full bash access
- `cwd = /` — the AI can run any command on the host
- No `GIT_URL` — master doesn't clone a repo
- Sends a `container_list` frame to mobile on every WebSocket connect
- Background poller (every 30s) queries Docker for managed containers and pushes updates

### Child containers

- Unchanged `claudulhu-server` instances
- Each has its own Noise keypair and port (default range 9100–9199, set via `CHILD_PORT_RANGE`)
- Labeled `claudulhu.managed=1` and `claudulhu.git_url=<url>` so master can discover them
- Mobile connects directly to each child via an independent Noise tunnel

### Mobile

- One saved connection: master's QR
- On connect to master: receives `container_list` frame → shows `ContainersBar` (horizontal scroll of chips)
- Tapping a chip: disconnects master tunnel, opens `ChildChatModal` with a direct Noise connection to that child
- Back from child: child tunnel disconnects, master tunnel re-establishes automatically
- Each child's chat history is cached separately in AsyncStorage under `child:<container_id>`

## Wire protocol additions (master only)

New server→client frames:

```
container_list   { containers: ContainerInfo[] }
container_status { id, name, status }
```

`ContainerInfo`:
```
{ id, name, git_url, status, host, port, pubkey }
```

These frames are intercepted by `ChatPane`'s `onContainerFrame` prop and routed to `AppInner` without touching chat message state.

## Pubkey discovery

Master runs `docker exec <container_name> claudulhu-server --print-pubkey` to get a child's Noise public key. Results are cached in `/data/pubkey_registry.json` (maps container ID → base32 pubkey) so exec is only run once per container.

## Networking

All containers run on the `claudulhu-net` Docker bridge network (created by master's entrypoint on startup). Only master's Noise port (9000) is published to the host. Child Noise ports are reachable from outside for direct mobile connections.

## Deploying master

```sh
docker run -d \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v claudulhu-maitred-data:/data \
  -p 9000:9000 \
  -e ANTHROPIC_API_KEY=... \
  -e PUBLIC_HOST=<your-server-ip> \
  ghcr.io/georgebradford0/claudulhu-maitred:latest
```

Master creates the `claudulhu-net` network on startup. Child containers are created by asking master in chat (e.g. "start a server for github.com/owner/repo").

## Docker image

```
ghcr.io/georgebradford0/claudulhu-maitred:latest
```

Build (from repo root):
```sh
docker buildx build \
  --builder multiplatform \
  --platform linux/amd64,linux/arm64 \
  --push \
  -t ghcr.io/georgebradford0/claudulhu-maitred:X.Y.Z \
  -t ghcr.io/georgebradford0/claudulhu-maitred:latest \
  -f master/Dockerfile \
  .
```
