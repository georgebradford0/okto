#!/usr/bin/env bash
set -euo pipefail

# Local dev loop: build the lair image from the working tree (single-arch,
# loaded into the local Docker daemon) and run it against ./dev-data/.
# Children spawn inside the same container; their data lives in
# ./dev-data/agents on the host via bind mount.
#
# Stop with ./stop_dev.sh (docker rm -f octo-lair-dev + rms ./dev-data/).

REPO_ROOT="$(cd "$(dirname "$0")" && pwd)"
DEV_ROOT="${REPO_ROOT}/dev-data"
DEV_CONFIG_SRC="${REPO_ROOT}/config.json"
DEV_NOISE_PORT="${DEV_NOISE_PORT:-9000}"
DEV_HTTP_PORT="${DEV_HTTP_PORT:-9001}"
DEV_CONTAINER="${DEV_CONTAINER:-octo-lair-dev}"
DEV_IMAGE="${DEV_IMAGE:-octo-lair:dev}"

if [ ! -f "${DEV_CONFIG_SRC}" ]; then
    echo "ERROR: ${DEV_CONFIG_SRC} is missing." >&2
    echo "       Create it (gitignored) with the same schema as ~/.octo/config.json:" >&2
    echo "       { \"anthropic_api_key\": \"sk-ant-…\", \"model\": \"claude-sonnet-4-6\" }" >&2
    exit 1
fi

echo "▸ docker build -t ${DEV_IMAGE} (single-arch, host-arch only)..."
docker build -f "${REPO_ROOT}/lair/Dockerfile" -t "${DEV_IMAGE}" "${REPO_ROOT}"

mkdir -p "${DEV_ROOT}"
install -m 600 "${DEV_CONFIG_SRC}" "${DEV_ROOT}/config.json"

# Wipe any leftover container from a previous run.
docker rm -f "${DEV_CONTAINER}" >/dev/null 2>&1 || true

# Empty env file by default; override by editing ./dev-data/lair-env before
# (re)running start_dev.sh.
touch "${DEV_ROOT}/lair-env"

# Forward GH_TOKEN from the host shell into the container if present.
EXTRA_ENV=()
if [ -n "${GH_TOKEN:-}" ]; then
    EXTRA_ENV+=("-e" "GH_TOKEN=${GH_TOKEN}")
fi

echo "▸ docker run -d --name ${DEV_CONTAINER} ..."
echo "  data:       ${DEV_ROOT}"
echo "  noise port: ${DEV_NOISE_PORT}"
echo "  http port:  ${DEV_HTTP_PORT} (127.0.0.1 only)"
echo ""

docker run -d \
    --name "${DEV_CONTAINER}" \
    -p "${DEV_NOISE_PORT}:8443" \
    -p "127.0.0.1:${DEV_HTTP_PORT}:8000" \
    -v "${DEV_ROOT}:/data" \
    --env-file "${DEV_ROOT}/lair-env" \
    -e "PUBLIC_PORT=${DEV_NOISE_PORT}" \
    -e "OCTO_DEV=1" \
    "${EXTRA_ENV[@]}" \
    "${DEV_IMAGE}"

echo ""
echo "✓ Dev lair started. Tail logs with:"
echo "    docker logs -f ${DEV_CONTAINER}"
