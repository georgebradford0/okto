#!/usr/bin/env sh
set -e

# ── Required ──────────────────────────────────────────────────────────────────
if [ -z "$ANTHROPIC_API_KEY" ]; then
    echo "ERROR: ANTHROPIC_API_KEY is required" >&2
    exit 1
fi

if [ -z "$PUBLIC_HOST" ]; then
    if [ "${CLAUDULHU_DEV:-0}" = "1" ]; then
        PUBLIC_HOST="127.0.0.1"
        echo "[claudulhu-rulyeh] DEV mode: using PUBLIC_HOST=127.0.0.1"
    else
        PUBLIC_HOST=$(curl -sf --max-time 5 https://api.ipify.org || wget -qO- --timeout=5 https://api.ipify.org 2>/dev/null)
        if [ -z "$PUBLIC_HOST" ]; then
            echo "ERROR: Could not auto-detect public IP. Set PUBLIC_HOST explicitly." >&2
            exit 1
        fi
        echo "[claudulhu-rulyeh] Detected public IP: ${PUBLIC_HOST}"
    fi
fi
export PUBLIC_HOST

NOISE_PORT="${NOISE_PORT:-9000}"
CLAUDULHU_DATA_DIR="${CLAUDULHU_DATA_DIR:-/data}"
mkdir -p "$CLAUDULHU_DATA_DIR"

# ── Startup script ────────────────────────────────────────────────────────────
if [ -n "$STARTUP_SCRIPT" ]; then
    echo "[claudulhu-rulyeh] Running STARTUP_SCRIPT..."
    printf '%s' "$STARTUP_SCRIPT" | bash
    echo "[claudulhu-rulyeh] STARTUP_SCRIPT complete."
fi

# ── Noise key ─────────────────────────────────────────────────────────────────
NOISE_PUBKEY=$(claudulhu-rulyeh --print-pubkey)
echo "[claudulhu-rulyeh] Noise public key: ${NOISE_PUBKEY}"

# ── QR code ───────────────────────────────────────────────────────────────────
# Format v2: "2:<host>:<port>:<pubkey_base32>"
QR_DATA="2:${PUBLIC_HOST}:${NOISE_PORT}:${NOISE_PUBKEY}"
SENTINEL="[claudulhu-rulyeh] HTTP on"

PIPE=$(mktemp -t claudulhu-pipe-XXXXXX)
rm -f "$PIPE"
mkfifo "$PIPE"

claudulhu-rulyeh 2>&1 | tee "$PIPE" &
SERVER_PID=$!

QR_PRINTED=0
while IFS= read -r line; do
    if [ "$QR_PRINTED" -eq 0 ] && \
       printf '%s' "$line" | grep -qF "$SENTINEL"; then
        echo ""
        echo "[claudulhu-rulyeh] Scan this QR code with the app to connect:"
        echo ""
        printf '%s' "${QR_DATA}" | qrencode -l L -m 4 -t UTF8 -o -
        echo ""
        QR_PRINTED=1
    fi
done < "$PIPE"

rm -f "$PIPE"
wait "$SERVER_PID"
