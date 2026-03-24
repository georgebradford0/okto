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

SSH_PORT="${SSH_PORT:-2222}"

# ── SSH host key ───────────────────────────────────────────────────────────────
# Generate a fresh Ed25519 host keypair every startup so each session gets
# unique keys — the app re-scans the QR after a container restart.
rm -f /etc/ssh/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key.pub
ssh-keygen -t ed25519 -f /etc/ssh/ssh_host_ed25519_key -N "" -q

# ── SSH client key (for the mobile app) ───────────────────────────────────────
CLIENT_KEY_FILE=/tmp/claudulhu_client
rm -f "${CLIENT_KEY_FILE}" "${CLIENT_KEY_FILE}.pub"
ssh-keygen -t ed25519 -f "${CLIENT_KEY_FILE}" -N "" -q

mkdir -p /root/.ssh
chmod 700 /root/.ssh
# restrict to port-forwarding only — no shell, no X11, no agent forwarding
printf 'restrict,port-forwarding %s\n' "$(cat "${CLIENT_KEY_FILE}.pub")" \
    > /root/.ssh/authorized_keys
chmod 600 /root/.ssh/authorized_keys

# ── sshd config ───────────────────────────────────────────────────────────────
mkdir -p /var/run/sshd
cat > /etc/ssh/sshd_config << EOF
Port ${SSH_PORT}
HostKey /etc/ssh/ssh_host_ed25519_key
AuthorizedKeysFile /root/.ssh/authorized_keys
PasswordAuthentication no
ChallengeResponseAuthentication no
KbdInteractiveAuthentication no
UsePAM no
AllowTcpForwarding yes
GatewayPorts no
X11Forwarding no
PrintMotd no
AcceptEnv LANG LC_*
EOF

/usr/sbin/sshd -f /etc/ssh/sshd_config
echo "[claudulhu] SSH server listening on port ${SSH_PORT}"

# ── QR code ───────────────────────────────────────────────────────────────────
# hk: base64(raw 32-byte Ed25519 host public key)
#     Extracted from OpenSSH wire format: skip 4+11+4 = 19 bytes header
# ck: base64(raw 32-byte Ed25519 private key seed)
#     Extracted from OpenSSH binary blob: offset 125 =
#       15 (magic) + 8 (ciphername "none") + 8 (kdfname "none") + 4 (kdfoptions)
#       + 4 (num_keys) + 55 (pubkey blob w/ 4-byte len) + 4 (priv section len)
#       + 4 (checkint1) + 4 (checkint2) + 15 (keytype "ssh-ed25519" w/ 4-byte len)
#       + 4 (priv key len) = 125
HOST_PUB_KEY=$(awk '{print $2}' /etc/ssh/ssh_host_ed25519_key.pub \
    | base64 -d | dd bs=1 skip=19 count=32 2>/dev/null | base64 -w0)
CLIENT_PRIV_KEY=$(grep -v -- '-----' "${CLIENT_KEY_FILE}" | tr -d '\n' \
    | base64 -d | dd bs=1 skip=125 count=32 2>/dev/null | base64 -w0)

QR_DATA=$(printf '{"v":1,"host":"%s","port":%s,"hk":"%s","ck":"%s"}' \
    "${PUBLIC_HOST}" "${SSH_PORT}" "${HOST_PUB_KEY}" "${CLIENT_PRIV_KEY}")

echo ""
echo "[claudulhu] Scan this QR code with the app to connect:"
echo ""
printf '%s' "${QR_DATA}" | qrencode -l L -m 1 -t UTF8small -o -
echo ""

# Private key no longer needed on disk — app has it from the QR
rm -f "${CLIENT_KEY_FILE}" "${CLIENT_KEY_FILE}.pub"

# ── Git authentication ────────────────────────────────────────────────────────
if [ -n "$GIT_TOKEN" ]; then
    CLONE_URL=$(echo "$GIT_URL" | sed 's|https://\(.*@\)\?|https://'"$GIT_TOKEN"'@|')
else
    CLONE_URL="$GIT_URL"
    if [ -f /root/.ssh/id_rsa ]; then
        chmod 600 /root/.ssh/id_rsa
        ssh-keyscan github.com gitlab.com bitbucket.org >> /root/.ssh/known_hosts 2>/dev/null
    fi
fi

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

if [ -n "$GIT_TOKEN" ]; then
    git -C "$WORKSPACE" config credential.helper \
        "!f() { echo username=x-token; echo password=$GIT_TOKEN; }; f"
fi

# ── Write claudulhu config ────────────────────────────────────────────────────
mkdir -p /root/.claudulhu
printf '{"repo":"%s"}\n' "$WORKSPACE" > /root/.claudulhu/config.json

echo "[claudulhu] Starting server (repo: $WORKSPACE)"
exec claudulhu-server
