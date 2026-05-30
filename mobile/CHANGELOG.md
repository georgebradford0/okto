# Changelog — mobile

Changes to the okto mobile (React Native) client. Current version: see
`android/app/build.gradle` (`versionName`) — the single source of truth for the
mobile version.

Changelog tracking for the mobile client starts here. Earlier history lives in
the git log.

## [Unreleased]

### Changed

- **Migrated to the shared `@okto/ui` design system (gluestack-ui v3 + NativeWind).**
  Mobile and desktop now draw from one design-token set (via a new npm workspace),
  so the two clients read as one product. The color ramp moves from warm-paper to a
  modern neutral palette with a teal brand accent and crisp slate typography. The
  presentation layer (chat bubbles, tool chips, tasks sheet, input bar, headers,
  sidebar, connect/setup screens, modals) was reworked onto NativeWind utility
  classes + gluestack components (`GluestackUIProvider`, `Spinner`, `Button`),
  shrinking the bespoke `StyleSheet` from ~147 rules to ~27 (markdown text, the dark
  QR-scanner overlay, and the paper-plane/orbit marks). All streaming, camera,
  worktree, and connection behaviour is unchanged.
- Connection status indicator (pill) removed from headers. Connection health is now checked silently in the background; a modal only appears when the connection is lost, and auto-dismisses when the connection recovers.

### Fixed

- Deleted worktrees no longer reappear in the sidebar. The `useEffect` that fetches
  worktrees now aborts stale requests on cleanup, and `deleteWorktree` re-fetches the
  worktree list after the DELETE completes (matching the desktop pattern). Stale
  worktree entries for removed agents are also pruned.
- Keyboard is now dismissed before opening the tasks modal, preventing the keyboard
  from covering it.
