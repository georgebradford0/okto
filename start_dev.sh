#!/usr/bin/env bash
set -euo pipefail

DEV_IMAGE="lair:dev"

# ── Checks ─────────────────────────────────────────────────────────────────────
if [ -z "${ANTHROPIC_API_KEY_OCTO:-}" ]; then
    echo "ERROR: ANTHROPIC_API_KEY_OCTO is not set" >&2
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
docker build -f lair/Dockerfile -t "${DEV_IMAGE}" .

# ── Manifests ──────────────────────────────────────────────────────────────────
echo "▸ Applying namespace and RBAC..."
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/rbac.yaml

# ── Secret ─────────────────────────────────────────────────────────────────────
# Single Secret used by both the parent lair Deployment (via secretKeyRef) and
# every child pod created via lair's k8s tooling (via envFrom: lair-secrets).
# Keep these in sync if you add new keys: see k8s-ops/src/k8s.rs::upsert_secret
# for the production codepath that lair uses to mutate this same Secret.
echo "▸ Creating/updating lair-secrets..."
kubectl create secret generic lair-secrets \
    --from-literal=ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY_OCTO}" \
    --from-literal=GH_TOKEN="${GH_TOKEN:-}" \
    -n octo \
    --dry-run=client -o yaml | kubectl apply -f -

# ── Deployment ─────────────────────────────────────────────────────────────────
echo "▸ Applying lair deployment..."
kubectl apply -f k8s/lair.yaml

echo "▸ Switching to local image (imagePullPolicy: Never)..."
kubectl set image deployment/lair lair="${DEV_IMAGE}" -n octo
kubectl patch deployment lair -n octo \
    -p '{"spec":{"template":{"spec":{"containers":[{"name":"lair","imagePullPolicy":"Never"}]}}}}'

echo "▸ Setting OCTO_DEV=1..."
kubectl set env deployment/lair OCTO_DEV=1 -n octo

# ── Wait ───────────────────────────────────────────────────────────────────────
echo "▸ Waiting for lair pod to be ready..."
kubectl rollout status deployment/lair -n octo --timeout=120s

# ── Port-forward lair (background) ──────────────────────────────────────────
# Docker Desktop on Mac exposes NodePorts natively — no port-forward needed for
# child containers. We only forward lair's Noise port because DEV_CONN uses
# port 9000 rather than the NodePort (30090).
PID_FILE="/tmp/octo-dev-portforward.pid"

kubectl port-forward -n octo svc/lair-noise 9000:9000 >"/tmp/octo-portforward.log" 2>&1 &
echo $! > "${PID_FILE}"

echo ""
echo "✓ lair is running. Noise tunnel port-forwarded → localhost:9000"
echo "  Child containers are accessible via their NodePorts (30100–30199) directly."
echo "  Run ./stop_dev.sh to tear down."
echo ""
