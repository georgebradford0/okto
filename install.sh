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

echo "Killing existing app..."
pkill -9 -x claudulhu 2>/dev/null || true
sleep 1

echo "Installing to $APP_DEST..."
rm -rf "$APP_DEST"
cp -r "$APP_SRC" "$APP_DEST"

# Strip quarantine so macOS doesn't translocate the bundle to a random path
xattr -rd com.apple.quarantine "$APP_DEST" 2>/dev/null || true

# Force Launch Services to re-register the new bundle
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP_DEST"

# Force Finder to re-read the bundle (clears cached Get Info metadata)
killall Finder 2>/dev/null || true

INSTALLED_VERSION=$(defaults read "$APP_DEST/Contents/Info.plist" CFBundleShortVersionString 2>/dev/null || echo "unknown")
echo "Installed version: $INSTALLED_VERSION"

echo "Launching claudulhu..."
open "$APP_DEST"

echo "Done."
