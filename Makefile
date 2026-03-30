.PHONY: all build desktop dev dev-web

# Build everything
all: build desktop

# Build the Vite frontend
build:
	cd desktop && npm run build

# Build the desktop app (DMG installer)
desktop: build
	cd desktop && npm run tauri:build -- --bundles dmg

# Run the Tauri app in development mode
dev:
	cd desktop && npm run tauri:dev

# Run just the Vite dev server (browser mode, requires claudulhud running separately)
dev-web:
	cd desktop && npm run dev
