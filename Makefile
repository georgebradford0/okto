.PHONY: all daemon web desktop install-daemon copy-sidecar

TARGET := $(shell rustc -vV | awk '/host:/ { print $$2 }')

# Build everything
all: daemon web desktop

# Install the Python daemon via uv
daemon:
	cd claudulhu && uv tool install . --reinstall

# Build the web frontend
web:
	cd web && npm run build

# Copy claudulhud binary into Tauri sidecar directory and build the desktop app
desktop: daemon web copy-sidecar
	cd web && npm run tauri:build

copy-sidecar:
	cp $(shell which claudulhud) web/src-tauri/binaries/claudulhud-$(TARGET)

# Run the daemon (defaults to current directory as repo)
run:
	claudulhud --repo .

# Run the web dev server
dev-web:
	cd web && npm run dev
