#!/usr/bin/env bash
set -euo pipefail

# Tear down the dev lair started by start_dev.sh. Removes the container and
# wipes ./dev-data/.

REPO_ROOT="$(cd "$(dirname "$0")" && pwd)"
DEV_ROOT="${REPO_ROOT}/dev-data"
DEV_CONTAINER="${DEV_CONTAINER:-lair-dev}"

if docker inspect "${DEV_CONTAINER}" >/dev/null 2>&1; then
    echo "▸ docker rm -f ${DEV_CONTAINER}"
    docker rm -f "${DEV_CONTAINER}" >/dev/null
fi

if [ -d "${DEV_ROOT}" ]; then
    echo "▸ Removing ${DEV_ROOT}..."
    rm -rf "${DEV_ROOT}"
fi

echo ""
echo "✓ Dev environment stopped."
