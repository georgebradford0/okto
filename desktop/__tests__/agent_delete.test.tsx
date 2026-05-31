/**
 * Deleting a child agent elsewhere (e.g. from mobile) must not leave a ghost
 * chat behind on desktop. Lair's poller rebuilds the `agents` list from the
 * registry, so a deleted agent is simply absent from the next push. The
 * renderer reconciles against that push: it drops the agent's row, closes its
 * proxy socket, prunes its cached chat, and — if it was the active tab — falls
 * back to lair. Regression test for the "chat lingers, status stuck on Error"
 * bug where a deleted agent's transcript stayed on screen.
 *
 * The reopen-from-saved-session variant lives in agent_delete_reopen.test.tsx
 * (App reads localStorage at module-eval time, so that case needs to be the
 * first App require in its file).
 */
import {
  resetAll, connectMaster,
  screen, fireEvent, waitFor, act, wsFor,
} from './helpers/render'
import { FakeWebSocket } from './helpers/server'

const agentsFrame = (agents: unknown[]) => ({ type: 'agents', agents })
const alpha = { id: 'alpha', name: 'alpha', status: 'running', kind: 'local' }

beforeEach(() => resetAll())

test('a child deleted while active drops its chat and falls back to lair', async () => {
  const { ws: master } = await connectMaster()
  await act(async () => { master.mockServerEvent(agentsFrame([alpha])) })

  // Open the child and stream some content into it.
  fireEvent.click(await screen.findByTestId('sidebar-row-alpha'))
  await waitFor(() => expect(wsFor('/agents/alpha/stream')).toBeTruthy())
  const child = wsFor('/agents/alpha/stream')!
  await act(async () => {
    child.mockOpen()
    child.mockServerEvent({ type: 'ready', session_id: 'c1', resumed: false, model: 'sonnet' })
    child.mockServerEvent({ type: 'text', text: 'child says hi' })
  })
  expect(await screen.findByText('child says hi')).toBeInTheDocument()

  // Agent is deleted elsewhere — the next poller push omits it.
  await act(async () => { master.mockServerEvent(agentsFrame([])) })

  // The row is gone, the cached transcript is gone, and the child socket was
  // closed (so it can't keep streaming into a hidden slot).
  await waitFor(() => expect(screen.queryByTestId('sidebar-row-alpha')).not.toBeInTheDocument())
  expect(screen.queryByText('child says hi')).not.toBeInTheDocument()
  expect(child.readyState).toBe(FakeWebSocket.CLOSED)

  // The composer now routes to lair, not the dead child.
  fireEvent.change(screen.getByTestId('composer-input'), { target: { value: 'back to lair' } })
  fireEvent.click(screen.getByTestId('composer-send'))
  expect(master.frames().at(-1)).toEqual({ type: 'user_message', text: 'back to lair' })
})
