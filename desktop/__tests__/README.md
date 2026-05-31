# desktop behavioural tests

End-to-end behavioural tests for the desktop renderer. They mount the **real**
`<App/>` (`desktop/src/App.tsx`) under jsdom via `@testing-library/react` and
drive it through complete flows — connect → stream → chat — asserting on what
the user sees and what the client sends over the wire.

Run them with:

```sh
npm test -w desktop      # from the repo's JS workspace root
# or, from desktop/:
npx jest
```

## What's mocked

App.tsx only talks to a handful of non-DOM surfaces; everything else is real
(its own logic, `./qr`, `./wire`, `localStorage`, the DOM). Stand-ins live in
`jest.setup.cjs` + `__tests__/helpers/`:

| Surface | Stand-in |
|---------|----------|
| `@tauri-apps/api/core` `invoke` | jest mock; `noise_connect` resolves a fixed loopback port (`TUNNEL_PORT`) |
| `@tauri-apps/plugin-updater` / `plugin-process` | inert jest mocks (`check` → no update) |
| browser `WebSocket` | `FakeWebSocket` — tests push frames with `ws.mockServerEvent(...)` and read `ws.frames()` |
| `fetch` (`/history`, `/clear`, `/completions`, `/worktrees`) | `fakeFetch` router; override per-test with `onFetch(...)` |
| `@okto/ui` (Tamagui / react-native-web) | plain DOM primitives (`oktoUiMock.tsx`) |
| `lucide-react-native` | inert icon |

`localStorage` is the real jsdom one (cleared between tests).

## Helpers

- `helpers/server.ts` — `FakeWebSocket`, `fakeFetch`, `onFetch`, `resetServer`,
  `lastWs`/`wsFor`.
- `helpers/render.tsx` — `renderApp`, `connectMaster` (walks the app to a ready
  master chat), `resetAll` (per-test reset), `reloadApp` (re-evaluates App so it
  re-reads a seeded `localStorage` session), and `invokeMock`/`checkMock`.
- `helpers/oktoUiMock.tsx` — the @okto/ui → DOM shim.

Selectors come from `desktop/src/testIds.ts` (`data-testid`s wired into App.tsx).

## Files

- `App.test.tsx` — boot to the connect screen; update check on launch.
- `connection.test.tsx` — paste/validate connect string, tunnel failure,
  persistence, auto-connect from a saved session.
- `chat.test.tsx` — send, stream text + tool calls, done/error/interrupted,
  ping↔pong, clear.
- `sidebar.test.tsx` — child-agent roster, selecting a child opens its proxied
  stream and routes the composer to it.
- `tasks.test.tsx` — the background-task registry, drawer, and `cancel_task`.
- `interactions.test.tsx` — Enter-to-send and Disconnect.

The matching Rust transport test is `tests/tests/desktop.rs`
(`cargo test -p okto-tests --test desktop`).
