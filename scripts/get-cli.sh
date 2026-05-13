#!/usr/bin/env sh
set -e

REPO="georgebradford0/octo"
INSTALL_DIR="$HOME/.local/bin"

OS=$(uname -s)
ARCH=$(uname -m)

# octo is Linux-only — lair runs as a native process on the host you run
# `octo init` from, and the cloud-init it emits for remote agents is Linux
# too. Both x86_64 and aarch64 are supported.
case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64)  CLI_ARTIFACT="octo-linux-x86_64";  LAIR_ARTIFACT="octo-lair-linux-x86_64"  ;;
      aarch64) CLI_ARTIFACT="octo-linux-aarch64"; LAIR_ARTIFACT="octo-lair-linux-aarch64" ;;
      *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
    esac
    ;;
  *) echo "Unsupported OS: $OS (octo is Linux-only)"; exit 1 ;;
esac

mkdir -p "$INSTALL_DIR"

echo "Downloading $CLI_ARTIFACT..."
curl -fsSL "https://github.com/${REPO}/releases/latest/download/${CLI_ARTIFACT}" -o "$INSTALL_DIR/octo"
chmod +x "$INSTALL_DIR/octo"
echo "Installed to $INSTALL_DIR/octo"

echo "Downloading $LAIR_ARTIFACT..."
curl -fsSL "https://github.com/${REPO}/releases/latest/download/${LAIR_ARTIFACT}" -o "$INSTALL_DIR/octo-lair"
chmod +x "$INSTALL_DIR/octo-lair"
echo "Installed to $INSTALL_DIR/octo-lair"

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
echo "Next: run 'octo init --anthropic-api-key <key> --model <model>' to bootstrap"
echo "      lair on this host as a background process. Optional flags:"
echo "      --openai-api-key, --api-url, --env KEY=VALUE."
echo ""

"$INSTALL_DIR/octo" --help
