#!/usr/bin/env sh
set -e

REPO="georgebradford0/octo"
INSTALL_DIR="$HOME/.local/bin"

OS=$(uname -s)
ARCH=$(uname -m)

# octo is Linux-only — both x86_64 and aarch64 are supported. Only the CLI
# binary is installed here; `octo init` `docker pull`s the matching
# octo-lair image (ghcr.io/georgebradford0/octo-lair) on first run, so
# Docker must already be installed and runnable as this user.
case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64)  CLI_ARTIFACT="octo-linux-x86_64"  ;;
      aarch64) CLI_ARTIFACT="octo-linux-aarch64" ;;
      *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
    esac
    ;;
  *) echo "Unsupported OS: $OS (octo is Linux-only)"; exit 1 ;;
esac

mkdir -p "$INSTALL_DIR"

# Find the most recent `cli-v*` release tag via the GitHub releases API. No
# jq dependency — grep + sed is enough for the JSON shape we care about.
CLI_TAG=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases?per_page=50" \
  | grep -oE '"tag_name": *"[^"]+"' \
  | sed -E 's/"tag_name": *"([^"]+)"/\1/' \
  | grep -E '^cli-v' \
  | head -1)
if [ -z "$CLI_TAG" ]; then
  echo "Error: no cli-v* release found in ${REPO}" >&2
  exit 1
fi

echo "Downloading $CLI_ARTIFACT (${CLI_TAG})..."
curl -fsSL "https://github.com/${REPO}/releases/download/${CLI_TAG}/${CLI_ARTIFACT}" -o "$INSTALL_DIR/octo"
chmod +x "$INSTALL_DIR/octo"
echo "Installed to $INSTALL_DIR/octo"

# Warn if ~/.local/bin is not in PATH.
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "Add to your shell: export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac

# Install shell completions for octo.
DETECTED_SHELL=$(basename "${SHELL:-sh}")
COMPLETIONS_INSTALLED=""
case "$DETECTED_SHELL" in
  zsh)
    COMP_DIR="$HOME/.zfunc"
    COMP_FILE="$COMP_DIR/_octo"
    mkdir -p "$COMP_DIR"
    "$INSTALL_DIR/octo" completions zsh > "$COMP_FILE"
    echo "Zsh completions installed to $COMP_FILE"
    ZSHRC="$HOME/.zshrc"
    if ! grep -q 'fpath.*\.zfunc' "$ZSHRC" 2>/dev/null; then
      printf '\nfpath+=~/.zfunc\nautoload -Uz compinit && compinit\n' >> "$ZSHRC"
      echo "Added fpath and compinit to $ZSHRC"
    fi
    COMPLETIONS_INSTALLED=1
    ;;
  bash)
    COMP_FILE="$HOME/.local/share/bash-completion/completions/octo"
    mkdir -p "$(dirname "$COMP_FILE")"
    "$INSTALL_DIR/octo" completions bash > "$COMP_FILE"
    echo "Bash completions installed to $COMP_FILE"
    BASHRC="$HOME/.bashrc"
    SOURCE_LINE=". $COMP_FILE"
    if ! grep -qxF "$SOURCE_LINE" "$BASHRC" 2>/dev/null; then
      printf '\n%s\n' "$SOURCE_LINE" >> "$BASHRC"
      echo "Added completion source to $BASHRC"
    fi
    COMPLETIONS_INSTALLED=1
    ;;
  fish)
    COMP_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions"
    mkdir -p "$COMP_DIR"
    "$INSTALL_DIR/octo" completions fish > "$COMP_DIR/octo.fish"
    echo "Fish completions installed to $COMP_DIR/octo.fish"
    COMPLETIONS_INSTALLED=1
    ;;
  *)
    echo "Completions: run 'octo completions <bash|zsh|fish>' to generate for your shell."
    ;;
esac

echo ""
if [ -n "$COMPLETIONS_INSTALLED" ]; then
  echo "Tab-completions are installed but won't be active in this shell session."
  echo "Start a new shell (or run 'exec $DETECTED_SHELL') to activate them."
  echo ""
fi
if ! command -v docker >/dev/null 2>&1; then
  echo "WARNING: 'docker' is not on PATH. Install Docker Engine before running"
  echo "         'octo init' — it pulls and runs ghcr.io/georgebradford0/octo-lair."
  echo ""
fi

echo "Next: run 'octo init' to bootstrap lair on this host. It will prompt"
echo "      for API keys interactively, then 'docker pull' the lair image and"
echo "      'docker run' it. Optional init flags: --env KEY=VALUE, --image,"
echo "      --noise-port, --mcp-config."
echo ""

"$INSTALL_DIR/octo" --help
