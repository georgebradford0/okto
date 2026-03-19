# Sidecar binaries

Tauri expects the `claudulhud` binary here, named with the target triple suffix.

To populate for a local macOS build:

```bash
# Get the target triple
TARGET=$(rustc -vV | awk '/host:/ { print $2 }')

# Copy the installed claudulhud binary
cp $(which claudulhud) claudulhud-$TARGET
```

Example filename: `claudulhud-aarch64-apple-darwin`

The binary is excluded from git (see .gitignore). Run the copy step before
each `tauri build`.
