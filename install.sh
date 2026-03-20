#!/bin/bash
set -e

echo "Building claudulhu..."
make desktop

APP_SRC="desktop/src-tauri/target/release/bundle/macos/claudulhu.app"
APP_DEST="/Applications/claudulhu.app"

if [ ! -d "$APP_SRC" ]; then
  echo "Error: build output not found at $APP_SRC"
  exit 1
fi

echo "Uninstalling existing app..."
pkill -x claudulhu 2>/dev/null || true
rm -rf "$APP_DEST"

echo "Installing to $APP_DEST..."
cp -r "$APP_SRC" "$APP_DEST"

echo "Done. claudulhu installed to /Applications."
