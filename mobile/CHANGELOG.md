# Changelog — mobile

Changes to the okto mobile (React Native) client. Current version: see
`android/app/build.gradle` (`versionName`) — the single source of truth for the
mobile version.

Changelog tracking for the mobile client starts here. Earlier history lives in
the git log.

## [Unreleased]

### Changed

- Connection status indicator (pill) removed from headers. Connection health is now checked silently in the background; a modal only appears when the connection is lost, and auto-dismisses when the connection recovers.

### Fixed

- Deleted worktrees no longer reappear in the sidebar. The `useEffect` that fetches
  worktrees now aborts stale requests on cleanup, and `deleteWorktree` re-fetches the
  worktree list after the DELETE completes (matching the desktop pattern). Stale
  worktree entries for removed agents are also pruned.
- Keyboard is now dismissed before opening the tasks modal, preventing the keyboard
  from covering it.
