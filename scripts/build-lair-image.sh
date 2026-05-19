#!/usr/bin/env bash
set -euo pipefail

# Build & push the multi-arch lair image to ghcr.io.
#
# Image releases live outside CI by design (memory: GHA workflow was removed).
# Run this on a Linux + Docker host with buildx + a builder that supports
# linux/amd64 and linux/arm64 (a `docker buildx create --use --bootstrap`
# default-named builder is enough).
#
# Prerequisites:
#   * Logged into ghcr (`echo $GH_TOKEN | docker login ghcr.io -u USERNAME --password-stdin`).
#   * On a machine with QEMU registered for cross-arch (Docker Desktop ships
#     this; on bare Linux: `docker run --privileged tonistiigi/binfmt --install all`).
#
# Usage:
#   scripts/build-lair-image.sh             # tags :<version> and :latest from lair/Cargo.toml
#   scripts/build-lair-image.sh --no-push   # local build only, no push (loads :latest into the default builder cache)
#   scripts/build-lair-image.sh v0.9.1      # override the version tag

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="${OCTO_LAIR_IMAGE:-ghcr.io/georgebradford0/lair}"
PLATFORMS="${PLATFORMS:-linux/amd64,linux/arm64}"

PUSH=1
VERSION=""
for arg in "$@"; do
    case "$arg" in
        --no-push) PUSH=0 ;;
        v*|[0-9]*) VERSION="${arg#v}" ;;
        *) echo "usage: $0 [--no-push] [VERSION]" >&2; exit 2 ;;
    esac
done

if [ -z "$VERSION" ]; then
    VERSION=$(grep '^version' "${REPO_ROOT}/lair/Cargo.toml" | head -1 | sed -E 's/version *= *"([^"]+)".*/\1/')
fi

if [ -z "$VERSION" ]; then
    echo "Could not determine lair version from lair/Cargo.toml" >&2
    exit 1
fi

echo "▸ Building $IMAGE:$VERSION (+ :latest) for $PLATFORMS"

PUSH_FLAG="--push"
if [ "$PUSH" -eq 0 ]; then
    # Without --push, buildx can't natively load a multi-arch image into the
    # local Docker daemon. Drop to a single-arch build for local-only use.
    PUSH_FLAG="--load"
    PLATFORMS="linux/$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')"
    echo "  (--no-push: narrowing to $PLATFORMS and --load into local daemon)"
fi

cd "$REPO_ROOT"
docker buildx build \
    --platform "$PLATFORMS" \
    -f lair/Dockerfile \
    -t "$IMAGE:$VERSION" \
    -t "$IMAGE:latest" \
    $PUSH_FLAG \
    .
