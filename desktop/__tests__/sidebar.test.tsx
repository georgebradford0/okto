/**
 * Sidebar / multi-agent behaviour: lair pushes the child-agent roster over the
 * master stream, the rows render, and selecting a child opens its lair-proxied
 * stream so the composer routes to that child.
 */
import { resetAll, connectMaster, screen, fireEvent, waitFor, act } from './helpers/render'
import { wsFor } from './helpers/server'

beforeEach(() => resetAll())

const agentsFrame = (over = {}) => ({
  type: 'agents',
  agents: [{ id: 'alpha', name: 'alpha', status: 'running', kind: 'local', ...over }],
})

test('renders child agents pushed by lair', async () => {
  const { ws } = await connectMaster()
  await act(async () => {
    ws.mockServerEvent(agentsFrame())
  })
  expect(await screen.findByTestId('sidebar-row-alpha')).toBeInTheDocument()
})

test('selecting a child opens its proxied stream and routes the composer to it', async () => {
  const { ws: master } = await connectMaster()
  await act(async () => {
    master.mockServerEvent(agentsFrame())
  })

  fireEvent.click(await screen.findByTestId('sidebar-row-alpha'))

  // Selecting a child opens a WebSocket to lair's per-agent proxy path.
  await waitFor(() => expect(wsFor('/agents/alpha/stream')).toBeTruthy())
  const child = wsFor('/agents/alpha/stream')!
  await act(async () => {
    child.mockOpen()
    child.mockServerEvent({ type: 'ready', session_id: 'c1', resumed: false, model: 'sonnet' })
  })

  // A message now goes to the child's socket, not the master's.
  fireEvent.change(screen.getByTestId('composer-input'), { target: { value: 'hi child' } })
  fireEvent.click(screen.getByTestId('composer-send'))
  expect(child.frames().at(-1)).toEqual({ type: 'user_message', text: 'hi child' })
})

test('a child stream event renders in the selected child chat', async () => {
  const { ws: master } = await connectMaster()
  await act(async () => {
    master.mockServerEvent(agentsFrame())
  })
  fireEvent.click(await screen.findByTestId('sidebar-row-alpha'))
  await waitFor(() => expect(wsFor('/agents/alpha/stream')).toBeTruthy())
  const child = wsFor('/agents/alpha/stream')!
  await act(async () => {
    child.mockOpen()
    child.mockServerEvent({ type: 'ready', session_id: 'c1', resumed: false, model: 'sonnet' })
    child.mockServerEvent({ type: 'text', text: 'child says hi' })
  })
  expect(await screen.findByText('child says hi')).toBeInTheDocument()
})
