// Shared rendering helpers for the behavioural suites. `renderApp` mounts the
// real <App/>; `connectMaster` drives it through the full setup → tunnel →
// /history → /stream handshake so a test starts from a live, ready master chat.
//
// Web counterpart of mobile/__tests__/helpers/render.tsx. Two desktop-specific
// wrinkles vs mobile:
//   1. The transport is the `noise_connect` Tauri command (mocked to resolve a
//      fixed loopback port), not a native module — so we drive `invoke`.
//   2. App.tsx reads its persisted session from `localStorage` *synchronously
//      at module-eval time*. So `resetAll()` calls `jest.resetModules()` and a
//      test that wants auto-connect seeds `localStorage` BEFORE `renderApp()`,
//      which `require()`s a fresh App against the just-reset module registry.

import React from 'react'
import { render, screen, fireEvent, waitFor, act } from '@testing-library/react'
import { lastWs, wsFor, resetServer, TUNNEL_PORT, FakeWebSocket } from './server'

export const VALID_CONNECT = '2:10.0.0.5:9000:ABCDEFGHIJKLMNOP'

/** The current-registry `invoke` jest.fn (re-fetched after resetModules). */
export const invokeMock = (): jest.Mock =>
  require('@tauri-apps/api/core').invoke as jest.Mock

/** The current-registry updater `check` jest.fn. */
export const checkMock = (): jest.Mock =>
  require('@tauri-apps/plugin-updater').check as jest.Mock

/**
 * Reset everything between tests: network doubles, localStorage, the module
 * registry, and the Tauri-bridge mocks (back to their happy-path defaults).
 * Run this in `beforeEach`; configure overrides AFTER it, then `renderApp()`.
 */
function installMockDefaults() {
  const invoke = invokeMock()
  invoke.mockReset()
  invoke.mockImplementation((cmd: string) =>
    cmd === 'noise_connect' || cmd === 'noise_active_port'
      ? Promise.resolve(TUNNEL_PORT)
      : Promise.resolve(undefined),
  )
  const check = checkMock()
  check.mockReset()
  check.mockResolvedValue(null)
}

export function resetAll() {
  resetServer()
  localStorage.clear()
  // NB: deliberately NOT calling jest.resetModules() here. App is imported once
  // per test file and shares React with @testing-library/react; resetting the
  // registry every test would give App a *second* copy of React, so RTL's
  // `act` would no longer flush App's effects (breaking any effect-dependent
  // assertion). Per-test isolation comes from re-rendering a fresh <App/> +
  // RTL's afterEach cleanup. Only `reloadApp()` resets modules, for the one
  // case that needs App to re-read localStorage at module-eval time.
  installMockDefaults()
}

export function renderApp() {
  const App = require('../../src/App').default
  return render(React.createElement(App))
}

/** Push a `ready` frame and let React flush. */
export async function sendReady(ws: FakeWebSocket, resumed = false) {
  await act(async () => {
    ws.mockServerEvent({ type: 'ready', session_id: 's1', resumed, model: 'sonnet' })
  })
}

/**
 * Render the app and walk it to a ready master chat: paste a connect string,
 * wait for the Noise tunnel + /history to gate the /stream socket open, then
 * complete the WS handshake. Returns the live socket.
 */
export async function connectMaster(connectString = VALID_CONNECT) {
  const utils = renderApp()

  const input = await screen.findByTestId('qr-input')
  fireEvent.change(input, { target: { value: connectString } })
  fireEvent.click(screen.getByTestId('connect-btn'))

  // The /stream socket is only constructed after the tunnel resolves and
  // /history has loaded for the master baseUrl.
  await waitFor(() => expect(wsFor('/stream')).toBeTruthy())
  const ws = wsFor('/stream')!

  await act(async () => {
    ws.mockOpen()
  })
  await sendReady(ws)

  return { ...utils, ws }
}

export { screen, fireEvent, waitFor, act, lastWs, wsFor }
