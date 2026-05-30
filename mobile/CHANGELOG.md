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

- Pinned `react-native-reanimated` to `~4.3.0` and `react-native-worklets` to `~0.8.1`
  (resolving to reanimated 4.3.1 + worklets 0.8.3). The previous `^4.3.0`/`^0.8.1` ranges
  drifted apart — reanimated floated to 4.4.0 (which requires Worklets 0.9.x) while worklets
  stayed on 0.8.x, breaking the iOS CocoaPods install. A brief attempt to fix forward
  (worklets `^0.9.0` + reanimated 4.4.0) built but crashed at runtime on RN 0.84.1, so both
  are now pinned to the compatible 4.3.x/0.8.x pair (reanimated 4.3.x peer-requires Worklets
  0.8.x). Root lockfile regenerated to match.
- Fixed an "Invalid hook call / more than one copy of React" runtime crash (null hooks
  dispatcher in `useSharedValue` under `KeyboardProvider`). In the npm workspace, desktop's
  `react: ^19.1.0` floated React/React-DOM to a newer patch (19.2.6) that npm hoisted to the
  workspace root, where the root-hoisted `react-native` bound its Fabric renderer to it —
  while the mobile app resolved its own pinned 19.2.3, yielding two React instances. Added a
  root `overrides` forcing `react`/`react-dom` to a single version (19.2.3, satisfying
  `react-native@0.84.1`'s `^19.2.3` peer), so Metro resolves one React copy across the app,
  react-native, reanimated, and worklets.
- Added `react-dom@19.2.3` to mobile. `react-aria` (pulled in transitively by gluestack-ui)
  statically `require`s `react-dom`, which only resolved before because an incidental copy
  was hoisted to the workspace root; once the React override removed it from the root, Metro
  could no longer resolve it. `react-dom` is dead code on native but must resolve at bundle
  time. Verified end-to-end: clean Metro bundle + native build + simulator launch with no
  runtime crash. The `useEffect` that fetches
  worktrees now aborts stale requests on cleanup, and `deleteWorktree` re-fetches the
  worktree list after the DELETE completes (matching the desktop pattern). Stale
  worktree entries for removed agents are also pruned.
- Keyboard is now dismissed before opening the tasks modal, preventing the keyboard
  from covering it.
