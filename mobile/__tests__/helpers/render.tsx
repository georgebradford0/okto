// Shared rendering helpers for the behavioural suites. `renderApp` mounts the
// real <App/>; `connectMaster` drives it through the full setup → tunnel →
// /history → /stream handshake so a test starts from a live, ready master chat.

import React from 'react'
import { render, screen, fireEvent, waitFor, act } from '@testing-library/react-native'
import App from '../../App'
import { lastWs, FakeWebSocket } from './server'

export const VALID_CONNECT = '2:10.0.0.5:9000:ABCDEFGHIJKLMNOP'

export function renderApp() {
  return render(<App />)
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

  const input = await screen.findByPlaceholderText('2:host:port:key')
  fireEvent.changeText(input, connectString)
  fireEvent.press(screen.getByText('connect'))

  // The /stream socket is only constructed after the tunnel resolves and
  // /history has loaded for the master baseUrl.
  await waitFor(() => expect(lastWs()).toBeTruthy())
  const ws = lastWs()

  await act(async () => {
    ws.mockOpen()
  })
  await sendReady(ws)

  return { ...utils, ws }
}

export { screen, fireEvent, waitFor, act, lastWs }
