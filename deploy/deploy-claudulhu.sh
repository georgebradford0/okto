#!/usr/bin/env bash
# deploy-claudulhu.sh — Launch claudulhu-server on a fresh EC2 instance
# Requires: aws CLI (configured), docker (only needed locally if you pre-pull)
#
# Usage:
#   ./deploy-claudulhu.sh \
#     --git-url   https://github.com/user/repo \
#     --api-key   sk-ant-... \
#    [--git-token ghp_...]         # for private repos
#    [--region    us-east-1]       # default: your aws cli default region
#    [--instance-type t3.micro]    # default: t3.micro
#    [--key-name  my-keypair]      # EC2 key pair name for SSH access (optional)
#    [--public-host 1.2.3.4]       # skip auto-detect of public IP (optional)

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────────
IMAGE="ghcr.io/georgebradford0/claudulhu-server:0.0.1"
INSTANCE_TYPE="t3.micro"
REGION="${AWS_DEFAULT_REGION:-$(aws configure get region 2>/dev/null || echo us-east-1)}"
GIT_URL=""
API_KEY=""
GIT_TOKEN=""
KEY_NAME=""
PUBLIC_HOST=""

# ── Argument parsing ───────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    --git-url)       GIT_URL="$2";       shift 2 ;;
    --api-key)       API_KEY="$2";       shift 2 ;;
    --git-token)     GIT_TOKEN="$2";     shift 2 ;;
    --region)        REGION="$2";        shift 2 ;;
    --instance-type) INSTANCE_TYPE="$2"; shift 2 ;;
    --key-name)      KEY_NAME="$2";      shift 2 ;;
    --public-host)   PUBLIC_HOST="$2";   shift 2 ;;
    *) echo "Unknown argument: $1" >&2; exit 1 ;;
  esac
done

# ── Validation ─────────────────────────────────────────────────────────────────
if [[ -z "$GIT_URL" ]]; then
  echo "ERROR: --git-url is required" >&2; exit 1
fi
if [[ -z "$API_KEY" ]]; then
  echo "ERROR: --api-key is required" >&2; exit 1
fi

# ── Derive name slug from git URL ─────────────────────────────────────────────
# https://github.com/owner/repo.git  →  owner-repo
# git@github.com:owner/repo.git      →  owner-repo
REPO_SLUG=$(echo "$GIT_URL" \
  | sed 's|.*[:/]\([^/]*/[^/]*\)$|\1|' \
  | sed 's|\.git$||' \
  | tr '[:upper:]' '[:lower:]' \
  | sed 's|[^a-z0-9]|-|g' \
  | sed 's|-\+|-|g; s|^-||; s|-$||')
INSTANCE_NAME="claudulhu-${REPO_SLUG}"
# Security group is shared across all claudulhu instances in the region —
# the rules are identical (port 9000) regardless of repo.
SG_NAME="claudulhu-server"

echo "[claudulhu] Region:        $REGION"
echo "[claudulhu] Instance type: $INSTANCE_TYPE"
echo "[claudulhu] Image:         $IMAGE"
echo "[claudulhu] Git URL:       $GIT_URL"
echo "[claudulhu] Instance name: $INSTANCE_NAME"
echo ""

# ── Resolve latest Amazon Linux 2023 AMI ──────────────────────────────────────
echo "[1/5] Resolving latest Amazon Linux 2023 AMI..."
AMI_ID=$(aws ec2 describe-images \
  --region "$REGION" \
  --owners amazon \
  --filters \
    "Name=name,Values=al2023-ami-*-x86_64" \
    "Name=state,Values=available" \
  --query "sort_by(Images, &CreationDate)[-1].ImageId" \
  --output text)
echo "      AMI: $AMI_ID"

# ── Create or reuse security group ────────────────────────────────────────────
echo "[2/5] Configuring security group '$SG_NAME'..."
SG_ID=$(aws ec2 describe-security-groups \
  --region "$REGION" \
  --filters "Name=group-name,Values=$SG_NAME" \
  --query "SecurityGroups[0].GroupId" \
  --output text 2>/dev/null || echo "None")

if [[ "$SG_ID" == "None" || -z "$SG_ID" ]]; then
  SG_ID=$(aws ec2 create-security-group \
    --region "$REGION" \
    --group-name "$SG_NAME" \
    --description "claudulhu-server: Noise proxy port" \
    --query "GroupId" \
    --output text)
  echo "      Created security group: $SG_ID"

  # Noise TCP proxy
  aws ec2 authorize-security-group-ingress \
    --region "$REGION" \
    --group-id "$SG_ID" \
    --protocol tcp --port 9000 --cidr 0.0.0.0/0 > /dev/null

  # SSH — only if a key pair was provided
  if [[ -n "$KEY_NAME" ]]; then
    aws ec2 authorize-security-group-ingress \
      --region "$REGION" \
      --group-id "$SG_ID" \
      --protocol tcp --port 22 --cidr 0.0.0.0/0 > /dev/null
  fi
else
  echo "      Reusing existing security group: $SG_ID"
fi

# ── Build user data script ─────────────────────────────────────────────────────
# Passed to EC2 as base64; runs as root on first boot.
# The Noise keypair is stored in a named Docker volume so it survives
# container restarts (the QR code stays valid as long as the volume exists).

PUBLIC_HOST_LINE=""
if [[ -n "$PUBLIC_HOST" ]]; then
  PUBLIC_HOST_LINE="-e PUBLIC_HOST=\"${PUBLIC_HOST}\" \\"
fi

GIT_TOKEN_LINE=""
if [[ -n "$GIT_TOKEN" ]]; then
  GIT_TOKEN_LINE="-e GIT_TOKEN=\"${GIT_TOKEN}\" \\"
fi

USER_DATA=$(cat <<USERDATA
#!/bin/bash
set -e
# Install Docker
dnf update -y
dnf install -y docker
systemctl enable --now docker

# Pull image
docker pull ${IMAGE}

# Run container
docker run -d \
  --name claudulhu-${REPO_SLUG} \
  --restart unless-stopped \
  -p 9000:9000 \
  -v claudulhu-keys-${REPO_SLUG}:/etc/claudulhu \
  -e GIT_URL="${GIT_URL}" \
  -e ANTHROPIC_API_KEY="${API_KEY}" \
  ${GIT_TOKEN_LINE}
  ${PUBLIC_HOST_LINE}
  ${IMAGE}
USERDATA
)

# ── Launch instance ────────────────────────────────────────────────────────────
echo "[3/5] Launching EC2 instance..."

LAUNCH_ARGS=(
  --region "$REGION"
  --image-id "$AMI_ID"
  --instance-type "$INSTANCE_TYPE"
  --security-group-ids "$SG_ID"
  --user-data "$USER_DATA"
  --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=${INSTANCE_NAME}},{Key=claudulhu-repo,Value=${REPO_SLUG}}]"
  --query "Instances[0].InstanceId"
  --output text
)

if [[ -n "$KEY_NAME" ]]; then
  LAUNCH_ARGS+=(--key-name "$KEY_NAME")
fi

INSTANCE_ID=$(aws ec2 run-instances "${LAUNCH_ARGS[@]}")
echo "      Instance ID: $INSTANCE_ID"

# ── Wait for instance to be running ───────────────────────────────────────────
echo "[4/5] Waiting for instance to reach 'running' state..."
aws ec2 wait instance-running --region "$REGION" --instance-ids "$INSTANCE_ID"

PUBLIC_IP=$(aws ec2 describe-instances \
  --region "$REGION" \
  --instance-ids "$INSTANCE_ID" \
  --query "Reservations[0].Instances[0].PublicIpAddress" \
  --output text)

echo "      Public IP: $PUBLIC_IP"

# ── Done ──────────────────────────────────────────────────────────────────────
echo ""
echo "[5/5] Instance is running. Docker is installing in the background (~2 min)."
echo ""
echo "┌─────────────────────────────────────────────────────────────────┐"
echo "│  claudulhu-server deployed                                      │"
echo "│                                                                 │"
printf  "│  Name:      %-52s│\n" "$INSTANCE_NAME"
printf  "│  Instance:  %-52s│\n" "$INSTANCE_ID"
printf  "│  Public IP: %-52s│\n" "$PUBLIC_IP"
printf  "│  Port:      %-52s│\n" "9000 (Noise TCP)"
echo "│                                                                 │"
echo "│  To retrieve the QR code (after ~2 min):                       │"
printf  "│    aws ec2 get-console-output --region %-26s│\n" "$REGION \\"
printf  "│      --instance-id %-45s│\n" "$INSTANCE_ID \\"
printf  "│      --latest --output text | grep -A 40 'Scan this QR'%-7s│\n" ""
echo "│                                                                 │"
if [[ -n "$KEY_NAME" ]]; then
printf  "│  SSH:  ssh ec2-user@%-44s│\n" "$PUBLIC_IP"
fi
echo "└─────────────────────────────────────────────────────────────────┘"
