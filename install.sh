#!/bin/bash
set -e

echo "Building claudulhu..."
touch desktop/src-tauri/src/main.rs
make desktop

DMG=$(find desktop/src-tauri/target/release/bundle/dmg -name "*.dmg" | head -1)
if [ -z "$DMG" ]; then
  echo "Error: DMG not found after build"
  exit 1
fi
echo "DMG: $DMG"

echo "Killing existing app..."
pkill -9 -x claudulhu 2>/dev/null || true
sleep 1

echo "Mounting DMG..."
VOLUME=$(hdiutil attach "$DMG" -nobrowse | awk 'END{print $3}')

echo "Installing to /Applications..."
rm -rf /Applications/claudulhu.app
cp -r "$VOLUME/claudulhu.app" /Applications/claudulhu.app
hdiutil detach "$VOLUME" -quiet

xattr -rd com.apple.quarantine /Applications/claudulhu.app 2>/dev/null || true
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f /Applications/claudulhu.app
killall Finder 2>/dev/null || true

INSTALLED_VERSION=$(defaults read /Applications/claudulhu.app/Contents/Info.plist CFBundleShortVersionString 2>/dev/null || echo "unknown")
echo "Installed version: $INSTALLED_VERSION"

echo "Launching claudulhu..."
open /Applications/claudulhu.app

echo "Done."
