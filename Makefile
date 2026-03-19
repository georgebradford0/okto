.PHONY: all web desktop dev dev-web

# Build everything
all: web desktop

# Build the web frontend
web:
	cd web && npm run build

# Build the desktop app (.app bundle)
desktop: web
	cd web && npm run tauri:build -- --bundles app

# Run the Tauri app in development mode
dev:
	cd web && npm run tauri:dev

# Run just the Vite dev server (browser mode, requires claudulhud running separately)
dev-web:
	cd web && npm run dev
