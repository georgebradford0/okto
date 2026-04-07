# Changelog

## [Unreleased]

## [0.1.1] - 2026-04-06

### Changed
- **Session lifecycle enforced for all tool calls** — system prompt and tool descriptions now require `session_start` as the very first tool call and `session_end` as the very last, regardless of how many tools are used; previously "non-trivial work" wording left a loophole for single/quick calls

### Fixed
- **MCP child process detach** — replaced non-existent `child.forget()` with `std::mem::forget(child)` to correctly detach spawned MCP server processes from the tokio runtime

## [0.1.0] - 2026-04-06

### Added
- **Connection status dot in chat header** — 8×8 colored circle to the left of the "claudulhu" title indicates server connection state (green = ready, yellow = connecting/streaming, red = error)

### Fixed
- **Noise tunnel re-establishment on app foreground** — AppState listener in `AppInner` now calls `NoiseConnection.disconnect()` + `NoiseConnection.connect()` when the app resumes from background, fixing silent WebSocket reconnect failures caused by iOS suspending the native Noise TCP proxy

### Changed
- **Full server + mobile rewrite** — simplified the entire system end-to-end:
  - **Server (`server/src/main.rs`)**: single session, new wire protocol (`history` / `token` / `tool` / `question` / `done` / `error`), live event buffer with generation counter for safe reconnect replay, `deliver_current` flag prevents duplicate delivery when history already contains a completed response. Removed: worker sessions, session IDs in URLs, event log (.jsonl), seq tracking, `/workers` route, UUID usage, per-session HashMaps
  - **Mobile (`mobile/App.tsx`)**: rewritten from ~1,600 lines to ~680; simplified types (`Message`, `ConnStatus`, `ServerFrame`); three clear screens (connecting spinner, connection picker, chat); token accumulation streams assistant replies inline; AsyncStorage cache per connection; `sendMessageRef` pattern retained to avoid stale closures
