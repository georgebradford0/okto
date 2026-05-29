# Changelog — mobile

Changes to the okto mobile (React Native) client. Current version: see
`android/app/build.gradle` (`versionName`) — the single source of truth for the
mobile version.

Changelog tracking for the mobile client starts here. Earlier history lives in
the git log.

## [Unreleased]

### Fixed

- Deleted worktrees no longer reappear in the sidebar. The `useEffect` that fetches
  worktrees now aborts stale requests on cleanup, and `deleteWorktree` re-fetches the
  worktree list after the DELETE completes (matching the desktop pattern). Stale
  worktree entries for removed agents are also pruned.
