# Changelog — mobile

Changes to the okto mobile (React Native) client. Current version: see
`android/app/build.gradle` (`versionName`) — the single source of truth for the
mobile version.

Changelog tracking for the mobile client starts here. Earlier history lives in
the git log.

## [Unreleased]

## [0.2.1] - 2026-06-02

### Changed

- Tool calls in the chat now render as plain monospace text instead of a
  bordered chip. The pulsing/queued status dots are gone; while a
  tool is running, a soft light band sweeps across the label text (a "shimmer")
  to signal live activity. Tapping the label still expands the streamed output.
- Consecutive tool calls now collapse into a single group so a long agentic loop
  no longer floods the transcript. While the loop runs, the group shows just the
  current step's shimmering label plus an `n/total` counter; once idle it shows a
  muted "N tool calls" summary. Tap to expand the full list (each step still
  expands independently for its output). A lone tool still renders inline.

### Fixed

- Finished tool calls now keep their friendly natural-language label ("Editing
  file (…)") instead of reverting to the raw tool name (`edit_file(…)`) after a
  history reload. `/history` tool rows now carry the same `display` phrase the
  live stream sent, and the client prefers it over the raw `name(arg)` text.
- Returning from the background (or connecting) mid agentic-loop no longer drops
  the completed turns since your last message. On reconnect the client loads
  `/history` and appends the rows one-per-tick; if the server's `ready`/replay for
  the in-flight turn arrived before that staggered append finished, the replay's
  shadow anchor snapshotted a half-applied list and `replay_end` then wiped every
  completed turn in between — they vanished while the current turn showed. The
  pending stagger queue is now folded into the anchor before the snapshot, so the
  swap is lossless.
- Sidebar worktree rows now update promptly when an agent tears a worktree down
  server-side (e.g. during a chat turn). Previously the sidebar only refetched a
  worktree list when the agent roster changed, and lair never pushes `agents` for
  worktree-only changes — so a removed worktree lingered until the next unrelated
  agents poll. The agent chat now refreshes its worktree list at each turn
  boundary (`done`/`interrupted`/`error`).
- A worktree you're actively viewing that gets removed (torn down server-side, or
  deleted from another client) no longer leaves a stuck "ghost" chat pane behind:
  the chat now falls back to the parent agent's chat once the worktree disappears
  from its authoritative list. Mirrors the desktop client's `reconcileWorktrees`.
- TestFlight uploads no longer fail export-compliance check 90592: set
  `ITSAppUsesNonExemptEncryption` to `false` (okto uses only standard, published
  algorithms — X25519/ChaCha20-Poly1305/SHA-256 for the Noise tunnel, Ed25519 SSH,
  TLS — which qualify for the encryption exemption). The previous `true` declared
  non-exempt encryption, which requires a compliance code that doesn't exist.
- De-flaked the "connection-lost modal" mobile e2e test: its dismissal `waitFor`
  raced the modal's deferred exit-animation unmount under jest, intermittently
  failing the iOS build's Jest gate on slower CI runners.

### Changed

- Send button now uses the Lucide `Send` icon (`lucide-react-native`) instead of the hand-drawn paper-plane.
- **Replaced the gluestack-ui v3 + NativeWind styling stack with Tamagui.** NativeWind v4
  doesn't apply styles under React Native's New Architecture (which RN 0.84 + reanimated 4
  require), so the mobile UI rendered unstyled. The shared `@okto/ui` package was rebuilt on
  Tamagui (a proven RN + web design system): the full okto token palette (light + dark) ported
  into a Tamagui theme, an `OktoProvider`, and `View`/`Text`/`Touchable`/`Spinner`/`Button`
  primitives. `App.tsx`'s ~150 NativeWind `className` sites were converted to Tamagui style
  props; NativeWind/Tailwind removed from the mobile build (babel, metro, global.css). Verified
  rendering styled on the iOS simulator (New Architecture). The `ErrorBoundary` fallback now uses
  RN primitives so it can render outside the provider.
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

- iOS release Archive (TestFlight) no longer fails on a missing Hermes compiler. The
  hermes-engine podspec baked an absolute `HERMES_CLI_PATH` to the npm-workspace-hoisted
  `hermes-compiler`, which resolved differently on CI vs local; a Podfile `post_install`
  hook now rewrites it to a `$(PODS_ROOT)`-relative path that points at the workspace-root
  `node_modules` in every environment.
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
