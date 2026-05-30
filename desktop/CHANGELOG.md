# Changelog — desktop

Changes to the okto desktop (Tauri) app. Current version: see `package.json` /
`src-tauri/tauri.conf.json`.

## [Unreleased]

### Fixed

- During streaming, the activity-indicator spinner now encircles the interrupt
  (stop) button instead of sitting beside it.

## [0.4.0] - 2026-05-30

### Changed

- **Redesigned UI on a shared design system.** The desktop and mobile clients
  now share one `@okto/ui` package (gluestack-ui v3 + NativeWind, consumed via a
  new npm workspace) so both read as one product. Desktop's chat shell, connect
  screen, sidebar, tasks drawer, and input bar were restyled to the shared design
  tokens — a modern neutral palette with a teal brand accent and crisp slate
  typography (replacing the warm-paper look). All connection/streaming/worktree
  behaviour is unchanged; only the presentation layer and `App.css` (removed) were
  touched. Rendered on react-native-web via Vite (`vite-plugin-rnw`).

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
