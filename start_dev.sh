#!/usr/bin/env bash
set -euo pipefail

DEV_IMAGE="rulyeh:dev"

# ── Checks ─────────────────────────────────────────────────────────────────────
if [ -z "${ANTHROPIC_API_KEY_CLAUDULHU:-}" ]; then
    echo "ERROR: ANTHROPIC_API_KEY_CLAUDULHU is not set" >&2
    exit 1
fi

if ! kubectl config get-contexts docker-desktop &>/dev/null; then
    echo "ERROR: docker-desktop kubectl context not found." >&2
    echo "       Enable Kubernetes in Docker Desktop → Settings → Kubernetes." >&2
    exit 1
fi

kubectl config use-context docker-desktop

# ── Build image locally ────────────────────────────────────────────────────────
echo "▸ Building local image ${DEV_IMAGE}..."
docker build -f rulyeh/Dockerfile -t "${DEV_IMAGE}" .

# ── Manifests ──────────────────────────────────────────────────────────────────
echo "▸ Applying namespace and RBAC..."
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/rbac.yaml

# ── Secret ─────────────────────────────────────────────────────────────────────
echo "▸ Creating/updating secret..."
kubectl create secret generic claudulhu-secrets \
    --from-literal=anthropic-api-key="${ANTHROPIC_API_KEY_CLAUDULHU}" \
    --from-literal=gh-token="${GH_TOKEN:-}" \
    -n claudulhu \
    --dry-run=client -o yaml | kubectl apply -f -

# ── Deployment ─────────────────────────────────────────────────────────────────
echo "▸ Applying rulyeh deployment..."
kubectl apply -f k8s/rulyeh.yaml

echo "▸ Switching to local image (imagePullPolicy: Never)..."
kubectl set image deployment/rulyeh rulyeh="${DEV_IMAGE}" -n claudulhu
kubectl patch deployment rulyeh -n claudulhu \
    -p '{"spec":{"template":{"spec":{"containers":[{"name":"rulyeh","imagePullPolicy":"Never"}]}}}}'

echo "▸ Setting CLAUDULHU_DEV=1..."
kubectl set env deployment/rulyeh CLAUDULHU_DEV=1 -n claudulhu

# ── Wait ───────────────────────────────────────────────────────────────────────
echo "▸ Waiting for rulyeh pod to be ready..."
kubectl rollout status deployment/rulyeh -n claudulhu --timeout=120s

# ── Port-forward rulyeh (background) ──────────────────────────────────────────
# Docker Desktop on Mac exposes NodePorts natively — no port-forward needed for
# child containers. We only forward rulyeh's Noise port because DEV_CONN uses
# port 9000 rather than the NodePort (30090).
PID_FILE="/tmp/claudulhu-dev-portforward.pid"

kubectl port-forward -n claudulhu svc/rulyeh-noise 9000:9000 &
echo $! > "${PID_FILE}"

echo ""
echo "✓ rulyeh is running. Noise tunnel port-forwarded → localhost:9000"
echo "  Child containers are accessible via their NodePorts (30100–30199) directly."
echo "  Run ./stop_dev.sh to tear down."
echo ""
