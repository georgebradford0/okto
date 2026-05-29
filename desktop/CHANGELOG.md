# Changelog — desktop

Changes to the okto desktop (Tauri) app. Current version: see `package.json` /
`src-tauri/tauri.conf.json`.

## [Unreleased]

### Changed

- **Tool-call chips drop the `Running`/`Pending` text prefix.** The prefix
  produced redundant labels like "Running Running …" where it blended into the
  description. Running vs. pending state is now conveyed solely by the pulsing
  dot (`tool-dot-pulse`) and the queued dot (`tool-dot-queued`); the chip shows
  just the tool description.
