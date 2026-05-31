/* eslint-disable no-undef */
// Global test environment for the desktop app's behavioural tests.
//
// App.tsx talks to four things the test must stand in for: the Tauri bridge
// (`invoke` for the Noise tunnel, the updater + process plugins), the shared
// @okto/ui design system, the lucide icon set, and the two network surfaces
// (browser WebSocket + fetch). jsdom already provides `window`, `document` and
// `localStorage`; we install controllable fakes for the rest so a test can
// render <App/> and drive a full connect → stream → chat flow.

const { FakeWebSocket, fakeFetch } = require('./__tests__/helpers/server')

// ── Network globals ───────────────────────────────────────────────────────────
global.WebSocket = FakeWebSocket
global.fetch = fakeFetch

// ── Tauri bridge → controllable jest mocks ────────────────────────────────────
// `invoke('noise_connect', …)` resolves a fixed loopback port; the renderer
// then opens a FakeWebSocket against `ws://127.0.0.1:<port>`. Defaults are
// (re)installed per test by render.tsx's `resetAll()`.
jest.mock('@tauri-apps/api/core', () => ({ invoke: jest.fn() }))
jest.mock('@tauri-apps/plugin-updater', () => ({ check: jest.fn() }))
jest.mock('@tauri-apps/plugin-process', () => ({ relaunch: jest.fn() }))

// ── Shared design-system (@okto/ui) → plain DOM primitives ────────────────────
jest.mock('@okto/ui', () => require('./__tests__/helpers/oktoUiMock'))

// ── lucide icons → inert ──────────────────────────────────────────────────────
jest.mock('lucide-react-native', () => ({ Send: () => null }))
