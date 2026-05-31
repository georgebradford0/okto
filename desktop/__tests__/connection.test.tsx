/**
 * Connection & onboarding flows: pasting a connect string, tunnel failure,
 * persistence, and auto-connect from a saved session. The transport is the
 * mocked `noise_connect` Tauri command.
 */
import {
  resetAll, renderApp, invokeMock, screen, fireEvent, waitFor, act, VALID_CONNECT,
} from './helpers/render'
import { wsFor } from './helpers/server'

const STORAGE_KEY = 'okto.desktop.state.v2'

beforeEach(() => resetAll())

describe('manual connect string', () => {
  test('rejects a malformed string and stays on the connect screen', async () => {
    renderApp()
    const input = await screen.findByTestId('qr-input')
    fireEvent.change(input, { target: { value: 'not-a-valid-string' } })
    fireEvent.click(screen.getByTestId('connect-btn'))

    expect(await screen.findByText(/Invalid QR payload/)).toBeInTheDocument()
    // No tunnel attempted; still on the connect screen.
    expect(invokeMock()).not.toHaveBeenCalledWith('noise_connect', expect.anything())
    expect(screen.getByTestId('qr-input')).toBeInTheDocument()
  })

  test('a valid string opens the tunnel and lands in the master chat', async () => {
    renderApp()
    const input = await screen.findByTestId('qr-input')
    fireEvent.change(input, { target: { value: VALID_CONNECT } })
    fireEvent.click(screen.getByTestId('connect-btn'))

    await waitFor(() =>
      expect(invokeMock()).toHaveBeenCalledWith('noise_connect', {
        host: '10.0.0.5',
        port: 9000,
        serverPubkeyB32: 'ABCDEFGHIJKLMNOP',
      }),
    )
    // The master /stream socket is constructed once the tunnel + /history gate
    // clears; completing its handshake flips the app into the chat shell.
    await waitFor(() => expect(wsFor('/stream')).toBeTruthy())
    await act(async () => {
      wsFor('/stream')!.mockOpen()
    })
    expect(await screen.findByTestId('composer-input')).toBeInTheDocument()
  })

  test('persists the connection so a later launch can auto-connect', async () => {
    renderApp()
    const input = await screen.findByTestId('qr-input')
    fireEvent.change(input, { target: { value: '2:1.2.3.4:9000:KEY' } })
    fireEvent.click(screen.getByTestId('connect-btn'))

    // The connected QR is only marked canonical (and thus persisted) once the
    // master WS handshake completes — mirror that before asserting on storage.
    await waitFor(() => expect(wsFor('/stream')).toBeTruthy())
    await act(async () => {
      wsFor('/stream')!.mockOpen()
    })

    await waitFor(
      () => expect(localStorage.getItem(STORAGE_KEY) ?? '').toContain('1.2.3.4'),
      { timeout: 2000 },
    )
  })
})

describe('tunnel failure', () => {
  test('surfaces the error and keeps the connect screen', async () => {
    invokeMock().mockImplementation((cmd: string) =>
      cmd === 'noise_connect'
        ? Promise.reject(new Error('handshake refused'))
        : Promise.resolve(45678),
    )
    renderApp()
    const input = await screen.findByTestId('qr-input')
    fireEvent.change(input, { target: { value: VALID_CONNECT } })
    fireEvent.click(screen.getByTestId('connect-btn'))

    expect(await screen.findByText(/handshake refused/)).toBeInTheDocument()
    expect(screen.getByTestId('qr-input')).toBeInTheDocument()
  })
})

// NB: auto-connect-from-saved-session lives in its own file (autoconnect.test.tsx)
// because App reads its persisted session synchronously at *module-eval* time —
// once App is required here (the tests above), that read is cached for the whole
// file, so the seeded-storage case needs a fresh module registry of its own.
