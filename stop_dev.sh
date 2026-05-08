#!/usr/bin/env bash
set -euo pipefail

PID_FILE="/tmp/octo-dev-portforward.pid"

if [ -f "${PID_FILE}" ]; then
    while IFS= read -r pid; do
        if kill -0 "${pid}" 2>/dev/null; then
            echo "▸ Stopping port-forward supervisor (pid ${pid})..."
            kill "${pid}" 2>/dev/null || true
        fi
    done < "${PID_FILE}"
    rm -f "${PID_FILE}"
fi

# Belt and suspenders: kill any orphaned `kubectl port-forward` that might
# have been left behind if the supervisor's signal didn't propagate.
if pgrep -f 'kubectl port-forward.*lair-noise' >/dev/null; then
    echo "▸ Reaping orphaned kubectl port-forward processes..."
    pkill -f 'kubectl port-forward.*lair-noise' 2>/dev/null || true
fi

echo "▸ Deleting all resources in octo namespace..."
kubectl delete namespace octo --ignore-not-found

# Sweep up PVs that were bound to octo claims and got left in Released/Failed
# state. Docker Desktop's hostpath provisioner sometimes ignores the Delete
# reclaim policy, which blocks the next dev run from binding a fresh PVC.
released_pvs=$(kubectl get pv \
    -o custom-columns='NAME:.metadata.name,NS:.spec.claimRef.namespace,PHASE:.status.phase' \
    --no-headers 2>/dev/null \
    | awk '$2 == "octo" && ($3 == "Released" || $3 == "Failed") { print $1 }')
if [ -n "${released_pvs}" ]; then
    echo "▸ Reaping released PVs from prior octo claims..."
    echo "${released_pvs}" | xargs kubectl delete pv
fi

echo ""
echo "✓ Dev environment stopped."
