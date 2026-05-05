#!/usr/bin/env sh
set -e

REPO="georgebradford0/octo"
BIN="octo"
INSTALL_DIR="$HOME/.local/bin"

OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64)  ARTIFACT="octo-linux-x86_64" ;;
      aarch64) ARTIFACT="octo-linux-aarch64" ;;
      *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
    esac
    ;;
  Darwin)
    case "$ARCH" in
      x86_64)  ARTIFACT="octo-macos-x86_64" ;;
      arm64)   ARTIFACT="octo-macos-aarch64" ;;
      *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
    esac
    ;;
  *) echo "Unsupported OS: $OS"; exit 1 ;;
esac

URL="https://github.com/${REPO}/releases/latest/download/${ARTIFACT}"

mkdir -p "$INSTALL_DIR"

echo "Downloading $ARTIFACT..."
curl -fsSL "$URL" -o "$INSTALL_DIR/$BIN"
chmod +x "$INSTALL_DIR/$BIN"

echo "Installed to $INSTALL_DIR/$BIN"

# Install k3s if not present (Linux only).
if [ "$OS" = "Linux" ] && ! command -v k3s > /dev/null 2>&1; then
  echo "k3s not found — installing..."
  curl -fsSL https://get.k3s.io | sh -
  # Copy kubeconfig to user location so kubectl/octo work without sudo.
  mkdir -p "$HOME/.kube"
  sudo cp /etc/rancher/k3s/k3s.yaml "$HOME/.kube/config"
  sudo chown "$(id -u):$(id -g)" "$HOME/.kube/config"
  chmod 600 "$HOME/.kube/config"
  echo "k3s installed. Kubeconfig written to $HOME/.kube/config"
fi

# Warn if ~/.local/bin is not in PATH.
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "Add to your shell: export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac

# Install shell completions.
DETECTED_SHELL=$(basename "${SHELL:-sh}")
case "$DETECTED_SHELL" in
  zsh)
    COMP_DIR="$HOME/.zfunc"
    mkdir -p "$COMP_DIR"
    "$INSTALL_DIR/$BIN" completions zsh > "$COMP_DIR/_octo"
    echo "Zsh completions installed to $COMP_DIR/_octo"
    ZSHRC="$HOME/.zshrc"
    if ! grep -q 'fpath.*\.zfunc' "$ZSHRC" 2>/dev/null; then
      printf '\nfpath+=~/.zfunc\nautoload -Uz compinit && compinit\n' >> "$ZSHRC"
      echo "Added fpath and compinit to $ZSHRC"
    fi
    ;;
  bash)
    COMP_FILE="$HOME/.local/share/bash-completion/completions/octo"
    mkdir -p "$(dirname "$COMP_FILE")"
    "$INSTALL_DIR/$BIN" completions bash > "$COMP_FILE"
    echo "Bash completions installed to $COMP_FILE"
    # Source the file directly from ~/.bashrc so it works even without the
    # bash-completion package (which is required for the XDG directory to be
    # picked up automatically).
    BASHRC="$HOME/.bashrc"
    if ! grep -q "octo" "$BASHRC" 2>/dev/null; then
      printf '\n. %s\n' "$COMP_FILE" >> "$BASHRC"
      echo "Added completion source to $BASHRC"
    fi
    ;;
  fish)
    COMP_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions"
    mkdir -p "$COMP_DIR"
    "$INSTALL_DIR/$BIN" completions fish > "$COMP_DIR/octo.fish"
    echo "Fish completions installed to $COMP_DIR/octo.fish"
    ;;
  *)
    echo "Completions: run 'octo completions <bash|zsh|fish>' to generate for your shell."
    ;;
esac

"$INSTALL_DIR/$BIN" --help
