# Changelog — desktop

Changes to the okto desktop (Tauri) app. Current version: see `package.json` /
`src-tauri/tauri.conf.json`.

## [Unreleased]

## [0.3.0] - 2026-05-29

### Added

- **In-app auto-update.** The app now checks for a newer release on launch
  (quietly) and exposes a **Check for updates** button in the chat toolbar.
  When an update is available the button turns into **↓ Update to vX.Y.Z**;
  clicking it downloads the signed update, installs it, and relaunches.
  Powered by Tauri's updater plugin against a signed `latest.json` manifest
  published to the `desktop-latest` GitHub release by `desktop.yml`. Update
  archives are verified against an embedded minisign public key, so only
  releases signed with the project's private key can be installed.

## [0.2.1] - 2026-05-29

### Changed

- **Tool-call chips drop the `Running`/`Pending` text prefix.** The prefix
  produced redundant labels like "Running Running …" where it blended into the
  description. Running vs. pending state is now conveyed solely by the pulsing
  dot (`tool-dot-pulse`) and the queued dot (`tool-dot-queued`); the chip shows
  just the tool description.
