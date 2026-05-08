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

echo ""
echo "✓ Dev environment stopped."
