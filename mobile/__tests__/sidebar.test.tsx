/**
 * The agent sidebar: rendering the pushed agent list, opening a child agent's
 * chat (proxied over the same tunnel), starting a stopped agent, terminating an
 * agent, and the git-worktree rows nested under an agent.
 */
import { Alert } from 'react-native'
import AsyncStorage from '@react-native-async-storage/async-storage'
import NoiseConnection from '../src/NativeNoiseConnection'
import { connectMaster, sendReady, screen, fireEvent, waitFor, act, lastWs } from './helpers/render'
import { resetServer, onFetch } from './helpers/server'

beforeEach(async () => {
  resetServer()
  await AsyncStorage.clear()
  ;(NoiseConnection.connect as jest.Mock).mockClear()
  ;(NoiseConnection.connect as jest.Mock).mockResolvedValue(45678)
})

const RUNNING = { id: 'lair-worker', name: 'lair-worker', status: 'running', kind: 'local' }
const STOPPED = { id: 'lair-idle', name: 'lair-idle', status: 'stopped', kind: 'local' }

async function pushAgents(ws: any, agents: any[]) {
  await act(async () => {
    ws.mockServerEvent({ type: 'agents', agents })
  })
}

async function openSidebar() {
  await act(async () => {
    fireEvent.press(screen.getByTestId('open-sidebar'))
  })
}

test('shows "No agents" until lair pushes an agent list', async () => {
  const { ws } = await connectMaster()
  await openSidebar()
  expect(screen.getByText('No agents')).toBeOnTheScreen()

  await pushAgents(ws, [RUNNING])
  // Display name strips the `lair-` prefix.
  expect(await screen.findByText('worker')).toBeOnTheScreen()
  expect(screen.queryByText('No agents')).toBeNull()
})

test('tapping a running agent opens its proxied chat', async () => {
  const { ws } = await connectMaster()
  await pushAgents(ws, [RUNNING])
  await openSidebar()

  await act(async () => {
    fireEvent.press(screen.getByText('worker'))
  })

  // The child pane opens a /stream WS to the agent's proxy path.
  await waitFor(() => expect(lastWs().url).toContain('/agents/lair-worker/stream'))
  const childWs = lastWs()
  await act(async () => {
    childWs.mockOpen()
  })
  await sendReady(childWs)

  // Header shows the agent name and the child composer is live. The master pane
  // is still mounted underneath, so two composers exist — the child is last.
  expect(await screen.findByText('worker')).toBeOnTheScreen()
  const composer = screen.getAllByPlaceholderText('message…').at(-1)!
  const send = screen.getAllByTestId('composer-send').at(-1)!

  // A message in the child goes out on the child socket.
  fireEvent.changeText(composer, 'hey agent')
  fireEvent.press(send)
  expect(childWs.frames().at(-1)).toEqual({ type: 'user_message', text: 'hey agent' })
})

test('routes by id and displays name when they differ (spaced display name)', async () => {
  const { ws } = await connectMaster()
  // id is a route-safe slug; name is the free-form display label.
  await pushAgents(ws, [{ id: 'my-agent', name: 'My Agent', status: 'running', kind: 'local' }])
  await openSidebar()

  // The sidebar shows the display name…
  await act(async () => {
    fireEvent.press(await screen.findByText('My Agent'))
  })

  // …but the proxy stream is opened at the id (slug), never the spaced name.
  await waitFor(() => expect(lastWs().url).toContain('/agents/my-agent/stream'))
  expect(lastWs().url).not.toContain('My Agent')
})

test('tapping a stopped agent starts it and shows the starting overlay', async () => {
  const { ws } = await connectMaster()
  await pushAgents(ws, [STOPPED])
  await openSidebar()

  await act(async () => {
    fireEvent.press(screen.getByText('idle'))
  })

  expect(ws.frames().at(-1)).toEqual({ type: 'start_agent', id: 'lair-idle' })
  expect(await screen.findByText('Starting container...')).toBeOnTheScreen()
})

test('long-pressing an agent confirms and emits terminate_agent', async () => {
  const alertSpy = jest.spyOn(Alert, 'alert')
  const { ws } = await connectMaster()
  await pushAgents(ws, [RUNNING])
  await openSidebar()

  await act(async () => {
    fireEvent(screen.getByText('worker'), 'longPress')
  })

  expect(alertSpy).toHaveBeenCalledWith(
    'Terminate agent?',
    expect.stringContaining('worker'),
    expect.any(Array),
  )

  // Invoke the destructive action the dialog offered.
  const buttons = alertSpy.mock.calls.at(-1)![2] as any[]
  const terminate = buttons.find(b => b.style === 'destructive')
  await act(async () => {
    terminate.onPress()
  })
  expect(ws.frames().at(-1)).toEqual({ type: 'terminate_agent', id: 'lair-worker' })
  alertSpy.mockRestore()
})

test('worktrees fetched for an agent render as nested rows and open scoped chats', async () => {
  // Match only the worktree *list* endpoint — a substring match would also
  // shadow the worktree chat's `.../worktrees/<id>/history` request.
  onFetch(
    url => url.endsWith('/worktrees'),
    () => [{ id: 'feature-x', branch: 'feature/x', path: '/w/feature-x', created_at: 0 }],
  )
  const { ws } = await connectMaster()
  await pushAgents(ws, [RUNNING])
  await openSidebar()

  expect(await screen.findByText('feature/x')).toBeOnTheScreen()
  expect(screen.getByText('worktree')).toBeOnTheScreen()

  await act(async () => {
    fireEvent.press(screen.getByText('feature/x'))
  })

  // Worktree chat is scoped to the worktree proxy path.
  await waitFor(() =>
    expect(lastWs().url).toContain('/agents/lair-worker/worktrees/feature-x/stream'),
  )
  // Header reads `<agent> / <branch>`.
  expect(await screen.findByText('worker / feature/x')).toBeOnTheScreen()
})

test('a worktree the agent tears down server-side drops from the sidebar at turn end', async () => {
  // lair never pushes `agents` for a worktree-only change, so the sidebar
  // relies on a turn-boundary refetch: the agent ran a tool that removed the
  // worktree, and once its turn ends the nesting must update without waiting
  // for an unrelated agents poll.
  let present = true
  onFetch(
    url => url.endsWith('/worktrees'),
    () => (present ? [{ id: 'feature-x', branch: 'feature/x', path: '/w/feature-x', created_at: 0 }] : []),
  )

  const { ws } = await connectMaster()
  await pushAgents(ws, [RUNNING])
  await openSidebar()
  expect(await screen.findByText('feature/x')).toBeOnTheScreen()

  // Open the agent's *main* chat (not the worktree chat) and complete the WS
  // handshake so the stream is live.
  await act(async () => { fireEvent.press(screen.getByText('worker')) })
  await waitFor(() => expect(lastWs().url).toContain('/agents/lair-worker/stream'))
  const childWs = lastWs()
  await act(async () => { childWs.mockOpen() })
  await sendReady(childWs)

  // The agent tears the worktree down mid-turn; its list now comes back empty.
  present = false
  // Turn boundary — no `agents` push, just `done` on the agent's own stream.
  await act(async () => { childWs.mockServerEvent({ type: 'done', cost_usd: 0 }) })

  // Reopen the sidebar from the agent chat: the worktree row is gone.
  await act(async () => { fireEvent.press(screen.getByTestId('open-sidebar-child')) })
  await waitFor(() => expect(screen.queryByText('feature/x')).toBeNull())
})

test('a worktree deleted while active drops its ghost chat and falls back to the agent', async () => {
  // The worktree we're *viewing* vanishes from the authoritative list (deleted
  // elsewhere, or torn down server-side). Its chat pane must not linger as a
  // ghost — we fall back to the parent agent's chat. Mirrors desktop's
  // reconcileWorktrees regression test.
  let present = true
  onFetch(
    url => url.endsWith('/worktrees'),
    () => (present ? [{ id: 'feature-x', branch: 'feature/x', path: '/w/feature-x', created_at: 0 }] : []),
  )

  const { ws } = await connectMaster()
  await pushAgents(ws, [RUNNING])
  await openSidebar()
  expect(await screen.findByText('feature/x')).toBeOnTheScreen()

  // Open the worktree's *own* chat and stream content.
  await act(async () => { fireEvent.press(screen.getByText('feature/x')) })
  await waitFor(() =>
    expect(lastWs().url).toContain('/agents/lair-worker/worktrees/feature-x/stream'),
  )
  const wtWs = lastWs()
  await act(async () => { wtWs.mockOpen() })
  await sendReady(wtWs)
  expect(await screen.findByText('worker / feature/x')).toBeOnTheScreen()

  // Worktree deleted elsewhere — the list now comes back empty. A turn
  // boundary on the worktree's own stream triggers the refetch + reconcile.
  present = false
  await act(async () => { wtWs.mockServerEvent({ type: 'done', cost_usd: 0 }) })

  // The ghost worktree chat is gone and we fell back to the parent agent's
  // chat (a fresh stream to the agent's own proxy path, not the worktree's).
  await waitFor(() => expect(screen.queryByText('worker / feature/x')).toBeNull())
  await waitFor(() =>
    expect(lastWs().url).toContain('/agents/lair-worker/stream'),
  )
  expect(lastWs().url).not.toContain('worktrees')
})
