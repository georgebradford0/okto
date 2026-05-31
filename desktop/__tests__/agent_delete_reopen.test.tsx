/**
 * Reopening the desktop app onto a saved session whose active tab is an agent
 * that was deleted while the app was closed must not show a ghost chat. App
 * reads its persisted session from localStorage *synchronously at module-eval
 * time*, so — like autoconnect.test.tsx — this lives in its own file: the first
 * renderApp() here is the first time App is required, so seeding localStorage
 * beforehand is enough (no jest.resetModules(), which would split App's React
 * from @testing-library's).
 *
 * The live-delete variant (agent deleted while the app is open and the tab is
 * active) is in agent_delete.test.tsx.
 */
import {
  resetAll, renderApp, invokeMock,
  screen, fireEvent, waitFor, act, wsFor,
} from './helpers/render'
import { onFetch } from './helpers/server'

const STORAGE_KEY = 'okto.desktop.state.v2'
const agentsFrame = (agents: unknown[]) => ({ type: 'agents', agents })

test('reopening onto a since-deleted active agent does not show a ghost chat', async () => {
  resetAll()
  // The proxied /history for the deleted agent fails (its child is gone), so
  // the restored cache would otherwise linger in an error state — exactly the
  // reported bug. Seed a saved session whose active tab is that agent BEFORE
  // the first renderApp (which reads localStorage at module-eval time).
  onFetch('/agents/alpha/history', () => Promise.reject(new Error('agent gone')))
  localStorage.setItem(STORAGE_KEY, JSON.stringify({
    qrPayload: { v: 2, host: '9.9.9.9', port: 9000, pk: 'SAVEDKEY' },
    activeAgent: 'alpha',
    itemsByAgent: { alpha: [{ id: 'g1', role: 'user', text: 'ghost message' }] },
  }))

  renderApp()
  await waitFor(() => expect(invokeMock()).toHaveBeenCalledWith('noise_connect', expect.anything()))
  await waitFor(() => expect(wsFor('/stream')).toBeTruthy())
  const master = wsFor('/stream')!
  await act(async () => {
    master.mockOpen()
    master.mockServerEvent({ type: 'ready', session_id: 's1', resumed: false, model: 'sonnet' })
  })

  // The restored ghost transcript is on screen for the deleted agent…
  expect(await screen.findByText('ghost message')).toBeInTheDocument()

  // …until lair's authoritative roster push (which omits the agent) reconciles
  // it away and falls back to lair.
  await act(async () => { master.mockServerEvent(agentsFrame([])) })
  await waitFor(() => expect(screen.queryByText('ghost message')).not.toBeInTheDocument())
  expect(screen.queryByText('Error')).not.toBeInTheDocument()

  // Composer is live against lair.
  fireEvent.change(screen.getByTestId('composer-input'), { target: { value: 'hello lair' } })
  fireEvent.click(screen.getByTestId('composer-send'))
  expect(master.frames().at(-1)).toEqual({ type: 'user_message', text: 'hello lair' })
})
