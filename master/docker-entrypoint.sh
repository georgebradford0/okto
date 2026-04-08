#!/usr/bin/env sh
set -e

# ── Required ──────────────────────────────────────────────────────────────────
if [ -z "$ANTHROPIC_API_KEY" ]; then
    echo "ERROR: ANTHROPIC_API_KEY is required" >&2
    exit 1
fi

if [ -z "$PUBLIC_HOST" ]; then
    PUBLIC_HOST=$(curl -sf --max-time 5 https://api.ipify.org || wget -qO- --timeout=5 https://api.ipify.org 2>/dev/null)
    if [ -z "$PUBLIC_HOST" ]; then
        echo "ERROR: Could not auto-detect public IP. Set PUBLIC_HOST explicitly." >&2
        exit 1
    fi
    echo "[claudulhu-master] Detected public IP: ${PUBLIC_HOST}"
fi

NOISE_PORT="${NOISE_PORT:-9000}"
CLAUDULHU_DATA_DIR="${CLAUDULHU_DATA_DIR:-/data}"
mkdir -p "$CLAUDULHU_DATA_DIR"

# ── Docker network ────────────────────────────────────────────────────────────
# Create the shared bridge network for master ↔ child container communication.
docker network create claudulhu-net 2>/dev/null || true
echo "[claudulhu-master] Docker network claudulhu-net ready"

# ── Noise key ─────────────────────────────────────────────────────────────────
NOISE_PUBKEY=$(claudulhu-master --print-pubkey)
echo "[claudulhu-master] Noise public key: ${NOISE_PUBKEY}"

# ── QR code ───────────────────────────────────────────────────────────────────
# Format v2: "2:<host>:<port>:<pubkey_base32>"
QR_DATA="2:${PUBLIC_HOST}:${NOISE_PORT}:${NOISE_PUBKEY}"
SENTINEL="[claudulhu-master] HTTP/WebSocket on"

PIPE=$(mktemp -t claudulhu-pipe-XXXXXX)
rm -f "$PIPE"
mkfifo "$PIPE"

claudulhu-master 2>&1 | tee "$PIPE" &
SERVER_PID=$!

QR_PRINTED=0
while IFS= read -r line; do
    if [ "$QR_PRINTED" -eq 0 ] && \
       printf '%s' "$line" | grep -qF "$SENTINEL"; then
        echo ""
        echo "[claudulhu-master] Scan this QR code with the app to connect:"
        echo ""
        printf '%s' "${QR_DATA}" | qrencode -l L -m 4 -t UTF8 -o -
        echo ""
        QR_PRINTED=1
    fi
done < "$PIPE"

rm -f "$PIPE"
wait "$SERVER_PID"
