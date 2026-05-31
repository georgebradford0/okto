/**
 * Auto-connect from a saved session. App reads its persisted session
 * synchronously at *module-eval* time, so this lives in its own file: the very
 * first `renderApp()` here is the first time App is required, so seeding
 * `localStorage` beforehand is enough — no `jest.resetModules()` (which would
 * give App a second React instance split from @testing-library/react's).
 */
import { resetAll, renderApp, invokeMock, screen, waitFor, act, wsFor } from './helpers/render'

const STORAGE_KEY = 'okto.desktop.state.v2'

test('connects on launch when a session is stored', async () => {
  resetAll()
  // Seed AFTER resetAll() (which clears storage) but BEFORE the first renderApp
  // (the first require of App, which reads this synchronously at eval time).
  localStorage.setItem(
    STORAGE_KEY,
    JSON.stringify({ qrPayload: { v: 2, host: '9.9.9.9', port: 9000, pk: 'SAVEDKEY' } }),
  )
  renderApp()

  await waitFor(() =>
    expect(invokeMock()).toHaveBeenCalledWith('noise_connect', {
      host: '9.9.9.9',
      port: 9000,
      serverPubkeyB32: 'SAVEDKEY',
    }),
  )

  // Reconnecting renders the chat shell directly (no connect-form flash); the
  // composer appears once the master stream handshake completes.
  await waitFor(() => expect(wsFor('/stream')).toBeTruthy())
  await act(async () => {
    wsFor('/stream')!.mockOpen()
    wsFor('/stream')!.mockServerEvent({ type: 'ready', session_id: 's1', resumed: false, model: 'sonnet' })
  })
  expect(await screen.findByTestId('composer-input')).toBeInTheDocument()
})
