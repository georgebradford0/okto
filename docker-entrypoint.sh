#!/usr/bin/env sh
set -e

# ── Required ──────────────────────────────────────────────────────────────────
if [ -z "$GIT_URL" ]; then
    echo "ERROR: GIT_URL is required" >&2
    exit 1
fi
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
    echo "[claudulhu] Detected public IP: ${PUBLIC_HOST}"
fi

NOISE_PORT="${NOISE_PORT:-9000}"

# ── Noise key ─────────────────────────────────────────────────────────────────
# Generate (or load) the server's static Curve25519 keypair and print the
# base32-encoded public key.  The key file is persisted across container
# restarts so the client only needs to re-scan the QR if the volume is lost.
mkdir -p /etc/claudulhu
NOISE_PUBKEY=$(claudulhu-server --print-pubkey)
echo "[claudulhu] Noise public key: ${NOISE_PUBKEY}"

# ── QR code ───────────────────────────────────────────────────────────────────
# Format v2: "2:<host>:<port>:<pubkey_base32>"
# All chars uppercase+digits+colon → QR alphanumeric mode → compact QR.
QR_DATA="2:${PUBLIC_HOST}:${NOISE_PORT}:${NOISE_PUBKEY}"

echo ""
echo "[claudulhu] Scan this QR code with the app to connect:"
echo ""
printf '%s' "${QR_DATA}" | qrencode -l L -m 4 -t UTF8 -o -
echo ""

# ── Git authentication ────────────────────────────────────────────────────────
# Allow token via env var or mounted secret file (/run/secrets/gh_token).
if [ -z "$GH_TOKEN" ] && [ -f /run/secrets/gh_token ]; then
    GH_TOKEN=$(cat /run/secrets/gh_token)
fi

case "$GIT_URL" in
  https://*)
    if [ -z "$GH_TOKEN" ]; then
        echo "ERROR: GH_TOKEN is required for HTTPS git URLs." >&2
        echo "  Pass -e GH_TOKEN=<token>  or  mount a secret:" >&2
        echo "  --mount type=secret,id=gh_token,target=/run/secrets/gh_token" >&2
        exit 1
    fi
    CLONE_URL=$(echo "$GIT_URL" | sed 's|https://\(.*@\)\?|https://'"$GH_TOKEN"'@|')
    ;;
  *)
    CLONE_URL="$GIT_URL"
    if [ -f /root/.ssh/id_rsa ]; then
        chmod 600 /root/.ssh/id_rsa
        ssh-keyscan github.com gitlab.com bitbucket.org >> /root/.ssh/known_hosts 2>/dev/null
    fi
    ;;
esac

# ── Clone or update repo ──────────────────────────────────────────────────────
WORKSPACE=/workspace
if [ -d "$WORKSPACE/.git" ]; then
    echo "[claudulhu] Updating existing repo at $WORKSPACE"
    git -C "$WORKSPACE" remote set-url origin "$CLONE_URL"
    git -C "$WORKSPACE" fetch --all
else
    echo "[claudulhu] Cloning $GIT_URL into $WORKSPACE"
    git clone "$CLONE_URL" "$WORKSPACE"
fi

GIT_USER_NAME="${GIT_USER_NAME:-claudulhu}"
GIT_USER_EMAIL="${GIT_USER_EMAIL:-claudulhu@localhost}"
git -C "$WORKSPACE" config user.name  "$GIT_USER_NAME"
git -C "$WORKSPACE" config user.email "$GIT_USER_EMAIL"

if [ -n "$GH_TOKEN" ]; then
    git -C "$WORKSPACE" config credential.helper \
        "!f() { echo username=x-token; echo password=$GH_TOKEN; }; f"
fi

# ── Write claudulhu config ────────────────────────────────────────────────────
mkdir -p /root/.claudulhu
REPO_NAME=$(basename "$GIT_URL" .git)
printf '{"repo":"%s","name":"%s"}\n' "$WORKSPACE" "$REPO_NAME" > /root/.claudulhu/config.json

echo "[claudulhu] Starting server (repo: $WORKSPACE)"
exec claudulhu-server
