#!/usr/bin/env bash
set -euo pipefail

PID_FILE="/tmp/claudulhu-dev-portforward.pid"

if [ -f "${PID_FILE}" ]; then
    while IFS= read -r pid; do
        if kill -0 "${pid}" 2>/dev/null; then
            echo "▸ Stopping port-forward (pid ${pid})..."
            kill "${pid}"
        fi
    done < "${PID_FILE}"
    rm -f "${PID_FILE}"
fi

echo "▸ Deleting all resources in claudulhu namespace..."
kubectl delete namespace claudulhu --ignore-not-found

echo ""
echo "✓ Dev environment stopped."
