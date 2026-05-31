# Changelog — desktop

Changes to the okto desktop (Tauri) app. Current version: see `package.json` /
`src-tauri/tauri.conf.json`.

## [Unreleased]

### Added
- End-to-end test suites for the desktop app. A jsdom + Jest behavioural suite
  (`desktop/__tests__/`, driven by `@testing-library/react`) renders the real
  `<App/>` over mocked Tauri / `WebSocket` / `fetch` boundaries and covers
  connect, chat streaming, tool calls, background tasks, the agent sidebar, and
  keyboard interactions; plus a Rust black-box test (`tests/tests/desktop.rs`)
  that drives the `noise_connect` transport path against a real lair over the
  Noise tunnel. Run with `npm test -w desktop` and
  `cargo test -p okto-tests --test desktop`.

### Fixed
- A child agent deleted elsewhere (e.g. from mobile) no longer leaves a ghost
  chat behind. When lair's roster push omits an agent, the renderer now closes
  its proxy socket, prunes its cached transcript, and falls back to the lair tab
  if it was active — previously the deleted agent's chat lingered on screen
  (absent from the sidebar) with the connection stuck on "Error", and the stale
  cache re-appeared on the next launch.

## [0.4.3] - 2026-05-30

### Changed

- Send button now uses the Lucide `Send` icon (`lucide-react-native`) instead of the `➤` glyph.

### Fixed

- Resolved the TypeScript build errors from the Tamagui migration (the `tsc` gate failed CI): text-style props (`fontSize`/`color`/`fontFamily`/`textAlign`) moved off `View`/`Touchable` onto `Text`, DOM-only attrs (`title`/`type`) removed, `overflow="auto"`→`"scroll"`, Spinner numeric `size`→`"large"`, and chat ref typing. Runtime unchanged.

## [0.4.2] - 2026-05-30

### Changed

- **Replaced the gluestack-ui v3 + NativeWind styling stack with Tamagui** (shared with mobile
  via `@okto/ui`). `App.tsx`'s DOM elements + NativeWind classes were converted to Tamagui
  primitives/props (div→View, span→Text, button→Touchable, onClick→onPress, ~70 className sites
  → style props incl. web `hover:`→`hoverStyle`); `main.tsx` now wraps the app in `OktoProvider`;
  Vite drops NativeWind (`TAMAGUI_TARGET=web`, react pinned to desktop's copy). Both `vite build`
  and the dev server pass; the connect screen renders styled. Some connected-view bits
  (dynamic `*_CLASS` maps, a few `input`/`textarea`) still carry no-op classNames pending a
  styling pass.

## [0.4.1] - 2026-05-30

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
