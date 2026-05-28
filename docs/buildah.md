# Building container images with Buildah

Reference for agents (and operators) building Docker / OCI images
inside the lair container. The image ships [Buildah](https://buildah.io)
configured for rootful (lair) and rootless (child agents) use; this
doc covers the recipes that actually work given that environment, plus
the Dockerfile patterns that don't fight the `vfs` storage driver.

Buildah is largely Docker-compatible — it consumes standard Dockerfiles
via `buildah bud` (= "build using dockerfile"). Most of what's below is
about the runtime constraints, not buildah-specific syntax.

---

## TL;DR — minimal recipe

```sh
# 1. Authenticate to the target registry. For GHCR with the GH_TOKEN
#    operator-set via `okto env set GH_TOKEN=...`:
echo "$GH_TOKEN" | buildah login -u <github-user> --password-stdin ghcr.io

# 2. Build (--push tags + pushes in one step so credentials only need
#    to be valid for one command):
buildah bud --push -t ghcr.io/<owner>/<image>:<tag> -f Dockerfile .
```

That's it for the happy path. Everything else is dealing with edge
cases: secrets, isolation, cross-arch, layer hygiene.

---

## Authentication

Buildah reads credentials from `$REGISTRY_AUTH_FILE` if set, else
`$XDG_RUNTIME_DIR/containers/auth.json`, else `$HOME/.config/containers/auth.json`,
in that order. `buildah login` writes whichever location it picks.
**Each agent has its own `$HOME` (set by lair to `/data/agents/<name>/`)**
so per-agent auth is naturally isolated — agent A logging into GHCR
does not give agent B credentials.

### Common registries

| Registry | Login command | Env var to use |
|---|---|---|
| `ghcr.io` | `echo "$GH_TOKEN" \| buildah login -u <gh-user> --password-stdin ghcr.io` | `GH_TOKEN` (already in lair-env) |
| `docker.io` | `buildah login -u <user> --password-stdin docker.io` | operator-set, e.g. `DOCKERHUB_TOKEN` |
| AWS ECR | `aws ecr get-login-password \| buildah login -u AWS --password-stdin <acct>.dkr.ecr.<region>.amazonaws.com` | `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` |
| Google Artifact Registry | `gcloud auth print-access-token \| buildah login -u oauth2accesstoken --password-stdin <region>-docker.pkg.dev` | `GOOGLE_APPLICATION_CREDENTIALS` pointing at a key file |

`/etc/containers/registries.conf` in the lair image already lists
`docker.io`, `ghcr.io`, and `quay.io` as unqualified search registries
— a bare `FROM ubuntu:24.04` resolves to `docker.io/library/ubuntu:24.04`
without extra config.

### Don't put credentials in a script

**Never hardcode tokens / passwords / connection strings into a build
script or Dockerfile.** Anything `COPY`d into the image is recoverable
by anyone who pulls it. Anything passed via `ARG`+`ENV` is visible to
`buildah inspect` and `cat /proc/<pid>/environ`. See the next section
for the correct pattern.

---

## Build-time secrets

Use buildah's `--secret` flag — the same shape as BuildKit's
`RUN --mount=type=secret,…`:

```sh
buildah bud \
    --secret id=mongo,env=MONGO_URI \
    --secret id=npm,src=$HOME/.npmrc \
    -t myimage:tag -f Dockerfile .
```

```dockerfile
# Secret is mounted only for the duration of this RUN; doesn't end up
# in any layer, history, or env.
RUN --mount=type=secret,id=mongo \
    MONGO=$(cat /run/secrets/mongo) && \
    ./scripts/seed-config-from-mongo.sh "$MONGO"

RUN --mount=type=secret,id=npm,target=/root/.npmrc \
    npm ci --omit=dev
```

Two delivery options for the secret value:
- **`id=foo,env=VAR`** — value comes from the build process's env var `VAR`.
- **`id=foo,src=/path/to/file`** — value comes from a file on disk.

### Runtime secrets (the common case)

If the secret is needed only when the **container runs** (database URI,
API key, etc.), don't involve the build at all. Inject at run time:

```sh
docker run -e MONGO_URI="$MONGO_URI" -e API_KEY="$API_KEY" myimage:tag
```

The Dockerfile should reference the env var by name (not bake a value
in). For Node.js / Python / Go that's literally just `process.env.MONGO_URI`
/ `os.environ['MONGO_URI']` / `os.Getenv("MONGO_URI")` — no build-time
plumbing needed.

### What not to do

| Anti-pattern | Why it's bad |
|---|---|
| `ARG SECRET` + `ENV SECRET=${SECRET}` | Value persists in image metadata + `/proc/<pid>/environ` of every container started from the image |
| `COPY my-key.json /app/key.json` | Anyone pulling the image gets the key |
| `RUN echo "$TOKEN" > .npmrc && npm install && rm .npmrc` | Token is in the layer between create and rm; recoverable via `buildah inspect` of the intermediate layer |
| Hardcoded values in shell scripts checked into git | Secret is in your git history forever |

---

## Choosing an isolation mode

Buildah uses one of three isolation strategies for `RUN` steps:

| Mode | Requires | Use when |
|---|---|---|
| `oci` (default) | Working runc/crun + capabilities the container has | Usually works inside the lair container without flags |
| `chroot` | `CAP_SYS_CHROOT` (root only) | Lair (root) when `oci` fails; never works for child agents |
| `rootless` | Subordinate uid/gid entries (already populated for every agent uid) | Child agents — the only mode they have permissions for besides `oci` |

Pick automatically based on the running uid:

```sh
ISO="$([ "$(id -u)" = "0" ] && echo chroot || echo rootless)"
buildah bud --isolation "$ISO" -t … -f Dockerfile .
```

Lair runs as root, so this resolves to `chroot` for lair builds and
`rootless` for agent builds — both supported, neither requires extra
flags or `docker run` cap changes.

The lair image bakes `/etc/subuid` + `/etc/subgid` entries for every
agent uid (okto-agent @ 10001, okto-agent-0..99 @ 10100..10199) with
65536 sub-ids each. Rootless mode "just works" with no extra setup.

---

## Dockerfile patterns for `vfs` storage

The image is configured with the `vfs` storage driver (see
`/etc/containers/storage.conf`) — it works inside any Docker container
without `/dev/fuse`, kernel overlay, or extra docker-run flags. The
trade-off is that every layer is a **full copy** of the working tree at
that point, not a copy-on-write delta. Builds are slower and chew more
disk than they would under `overlay`. Two practical implications:

### 1. Layer count is a real cost

A Dockerfile with 25 `RUN`/`COPY` layers takes ~5× longer to build than
the same content in 5 layers. Consolidate adjacent steps:

```dockerfile
# Bad — three layers, three full copies of the image state
RUN apt-get update
RUN apt-get install -y --no-install-recommends curl git
RUN rm -rf /var/lib/apt/lists/*

# Good — one layer
RUN apt-get update && \
    apt-get install -y --no-install-recommends curl git && \
    rm -rf /var/lib/apt/lists/*
```

Same idea for `npm install && npm rebuild && npm run build`, or
`pip install && python -m compileall`. Group operations that produce
the same logical state.

### 2. Avoid double-install patterns

A common anti-pattern under vfs is "install, then rm + reinstall to fix
platform binaries":

```dockerfile
# Wastes a full layer copy of node_modules
RUN npm install --include=dev
RUN rm -rf node_modules && npm install --include=dev
```

What you actually want is `npm rebuild` — it re-runs `postinstall`
hooks for the current platform without redownloading anything:

```dockerfile
RUN npm ci --include=dev && npm rebuild
```

(`npm ci` is also strictly better than `npm install` in a build —
deterministic from `package-lock.json`, faster, refuses to mutate the
lockfile.)

---

## Multi-stage builds

These work in buildah exactly as in Docker, and they're especially
valuable under vfs because they let you keep the **final pushed image**
small even though intermediate stages are big. Pattern:

```dockerfile
# Stage 1 — build tools, devDependencies, all the heavy lifting
FROM node:22-slim AS build
WORKDIR /app
COPY package*.json ./
RUN npm ci --include=dev
COPY . .
RUN npm run build

# Stage 2 — runtime-only. Copy just the artifacts forward.
FROM node:22-slim
WORKDIR /app
COPY package*.json ./
RUN npm ci --omit=dev
COPY --from=build /app/dist ./dist
EXPOSE 3000
CMD ["node", "dist/index.js"]
```

The pushed image contains only the runtime stage — no devDependencies,
no build tools, no source tree. The intermediate `build` stage exists
only in buildah's local vfs storage and never leaves the host.

### When to use `--squash`

If your Dockerfile is hopelessly chatty (base image alone has 50 layers
and you can't change that), pass `--squash` to collapse all build
layers into one final layer:

```sh
buildah bud --squash -t … -f Dockerfile .
```

Squash discards layer-level caching across builds but produces a
denser, smaller pushed image. Use sparingly — it's a last resort, not
a default.

---

## Cross-platform / multi-arch

**Buildah builds for the host CPU architecture by default.** It has no
native equivalent of `docker buildx --platform`. Two consequences:

1. **The lair container's host arch determines the output arch.** If lair is on a linux/arm64 EC2 instance, every `buildah bud` produces a linux/arm64 image, even if your fleet runs linux/amd64.
2. **Cross-arch builds require host-side QEMU + binfmt.** The lair container can't register binfmt itself; the EC2 host has to:

   ```sh
   # On the host, one-time setup:
   docker run --privileged --rm tonistiigi/binfmt --install amd64,arm64
   ```

   Then in lair you can `buildah bud --arch=amd64 …` and qemu-user
   transparently emulates the right instructions during the build.

For the common case ("the image will run on the same arch lair is
running on"), do nothing — just `buildah bud …`. For the
"lair-on-arm-build-for-amd64-fleet" case, gate the script:

```sh
HOST_ARCH=$(uname -m)
if [ "$HOST_ARCH" != "x86_64" ] && [ "${ALLOW_NON_AMD64_HOST:-0}" != "1" ]; then
    echo "Refusing to build a non-amd64 image. Either run on amd64 or" \
         "register binfmt on the host and set ALLOW_NON_AMD64_HOST=1." >&2
    exit 1
fi
```

---

## Storage and caching

Default rootless storage lives at `$HOME/.local/share/containers/storage`,
which for an agent named `<name>` maps to
`~/.okto/agents/<name>/.local/share/containers/storage` on the host
(via the bind mount). Storage is per-agent — agent A's pulled base
images and intermediate layers are not visible to agent B.

To control storage location explicitly (e.g. for ephemeral builds that
shouldn't persist):

```sh
buildah \
    --root /tmp/build-storage \
    --runroot /tmp/build-runroot \
    --storage-driver vfs \
    bud -t … -f Dockerfile .
```

Then `rm -rf /tmp/build-storage /tmp/build-runroot` at the end.

For long-running agents that build the same image repeatedly, **keep
the default storage** — buildah's per-layer cache will reuse identical
COPY/RUN steps across runs, even under vfs.

### Pruning

Periodic cleanup if the agent does many builds:

```sh
buildah rmi --prune     # remove dangling images (no tags, not referenced)
buildah rmi --all       # nuke everything — next build re-pulls base images
```

---

## Common pitfalls

| Symptom | Cause | Fix |
|---|---|---|
| `chroot: operation not permitted` | `--isolation chroot` as a non-root agent | Use `--isolation rootless` instead (or auto-pick based on uid) |
| `error creating build container: writing blob: storing blob to file: no space left` | vfs eating disk | Periodically `buildah rmi --prune`, or `--root /tmp/...` for ephemeral builds |
| `unable to get local issuer certificate` during base-image pull | Was an issue with the old Kaniko-based image; should be gone in lair 0.16+ | If it reappears, check `SSL_CERT_DIR` isn't set in env (it shouldn't be) |
| `level=error msg="Error pulling image..."` 401 | Not logged into the registry | `buildah login` first; check `$REGISTRY_AUTH_FILE` |
| `OCI runtime create failed: runc … no such file or directory` | `--isolation oci` can't find/use the runtime | Switch to `--isolation chroot` (root) or `--isolation rootless` (agent) |
| Build produces wrong architecture for target host | Host-arch mismatch (see Cross-platform section) | Register binfmt on the host, then `--arch=<target>` |
| Same Dockerfile builds in 30 s under Docker but 4 min under buildah | vfs vs overlay storage | Consolidate layers, use multi-stage, accept the trade-off — the runtime simplicity is the point |

---

## Template build script

A defensible starting point — auto-picks isolation, validates inputs,
cleans up auth file on exit, supports build-time secrets:

```sh
#!/usr/bin/env bash
set -euo pipefail

IMAGE="${IMAGE:?IMAGE is required, e.g. ghcr.io/owner/app}"
TAG="${TAG:-$(git rev-parse --short HEAD 2>/dev/null || date -u +%Y%m%d%H%M%S)}"
DOCKERFILE="${DOCKERFILE:-Dockerfile}"
CONTEXT="${CONTEXT:-.}"

: "${GH_TOKEN:?GH_TOKEN required for ghcr.io auth}"

# Auth file in a per-user-owned, non-shared dir.
AUTH_FILE="${XDG_RUNTIME_DIR:-/tmp}/buildah-auth-$$.json"
trap 'rm -f "$AUTH_FILE"' EXIT INT TERM
( umask 077 && : > "$AUTH_FILE" )
export REGISTRY_AUTH_FILE="$AUTH_FILE"

GH_USER="${GH_USER:-$(echo "$IMAGE" | awk -F/ '{print $2}')}"
echo "$GH_TOKEN" | buildah login -u "$GH_USER" --password-stdin ghcr.io

# Root = chroot (faster, no userns); non-root = rootless (only mode permitted)
ISOLATION="$([ "$(id -u)" = "0" ] && echo chroot || echo rootless)"

buildah bud \
    --isolation "$ISOLATION" \
    --layers \
    --pull \
    -t "$IMAGE:$TAG" \
    -t "$IMAGE:latest" \
    -f "$DOCKERFILE" \
    "$CONTEXT"

buildah push "$IMAGE:$TAG"     "docker://$IMAGE:$TAG"
buildah push "$IMAGE:latest"   "docker://$IMAGE:latest"

echo "Pushed $IMAGE:$TAG and $IMAGE:latest"
```

Flags worth knowing about that this template uses:

- **`--layers`** — keep intermediate layers in vfs storage for cache reuse across builds. Without it, every `RUN` is uncached.
- **`--pull`** — always check the registry for a newer base image rather than using a stale local copy. Drop if you want strict reproducibility on a pinned digest.
- **`--squash`** — *not* used here. Add if your final image is layer-bloated.

To add build-time secrets, append to the `buildah bud` call:

```sh
    --secret id=foo,env=FOO_SECRET \
    --secret id=bar,src=/path/to/file \
```

---

## See also

- [`docs/env-config.md`](env-config.md) — where `GH_TOKEN` and other operator-set env vars come from.
- [`docs/keypairs.md`](keypairs.md) — when you need `buildah login` for a private registry that uses SSH-key-backed auth.
- [Buildah project docs](https://github.com/containers/buildah/blob/main/docs/buildah-build.1.md) — full `buildah bud` man page.
