/**
 * Core chat behaviour over the /stream socket: sending a turn, streaming
 * assistant text and tool calls, turn terminators (done / error / interrupted),
 * keepalive, and clearing the conversation.
 */
import AsyncStorage from '@react-native-async-storage/async-storage'
import NoiseConnection from '../src/NativeNoiseConnection'
import { connectMaster, renderApp, screen, fireEvent, waitFor, act, lastWs } from './helpers/render'
import { resetServer, onFetch, fetchCalls } from './helpers/server'
import { VALID_CONNECT } from './helpers/render'

beforeEach(async () => {
  resetServer()
  await AsyncStorage.clear()
  ;(NoiseConnection.connect as jest.Mock).mockClear()
  ;(NoiseConnection.connect as jest.Mock).mockResolvedValue(45678)
})

// NB: do NOT wrap changeText+press in a single act() — React would batch them so
// the send handler's closure still sees the pre-change (empty) input. Each
// fireEvent self-wraps in act and flushes the re-render between the two events.
function typeAndSend(text: string) {
  fireEvent.changeText(screen.getByPlaceholderText('message…'), text)
  fireEvent.press(screen.getByTestId('composer-send'))
}

test('sending a message echoes a user bubble and emits a user_message frame', async () => {
  const { ws } = await connectMaster()
  typeAndSend('build me a feature')

  expect(await screen.findByText('build me a feature')).toBeOnTheScreen()
  expect(ws.frames().at(-1)).toEqual({ type: 'user_message', text: 'build me a feature' })
})

test('empty input does not send', async () => {
  const { ws } = await connectMaster()
  fireEvent.changeText(screen.getByPlaceholderText('message…'), '   ')
  fireEvent.press(screen.getByTestId('composer-send'))
  expect(ws.frames()).toEqual([])
})

test('streams assistant text deltas into a single bubble', async () => {
  const { ws } = await connectMaster()
  typeAndSend('hi')

  await act(async () => {
    ws.mockServerEvent({ type: 'text', text: 'Hello' })
    ws.mockServerEvent({ type: 'text', text: ', world' })
  })

  expect(await screen.findByText('Hello, world')).toBeOnTheScreen()
})

test('renders a tool call, reveals streamed output, then the final result', async () => {
  const { ws } = await connectMaster()
  typeAndSend('list files')

  await act(async () => {
    ws.mockServerEvent({
      type: 'tool_use',
      tool_use_id: 'tu1',
      tool: 'bash',
      input: { command: 'ls -la' },
      display: 'bash',
    })
    ws.mockServerEvent({ type: 'tool_output', tool_use_id: 'tu1', line: 'file-a.txt' })
  })

  // Tool label renders as `display (firstArg)`; output is collapsed until tapped.
  const toolRow = await screen.findByText('bash (ls -la)')
  expect(screen.queryByText(/file-a\.txt/)).toBeNull()

  await act(async () => {
    fireEvent.press(toolRow)
  })
  expect(await screen.findByText(/file-a\.txt/)).toBeOnTheScreen()

  // A tool_result supersedes the streamed lines with the final output.
  await act(async () => {
    ws.mockServerEvent({ type: 'tool_result', tool_use_id: 'tu1', output: 'exit 0' })
  })
  expect(await screen.findByText('exit 0')).toBeOnTheScreen()
})

test('a done frame seals the turn with its cost and returns to ready', async () => {
  // History echoes the turn so the end-of-turn reconcile is a no-op.
  onFetch('/history', () => ({
    messages: [
      { role: 'user', text: 'hi' },
      { role: 'assistant', text: 'Hello', cost_usd: 0.5 },
    ],
  }))
  const { ws } = await connectMaster()
  typeAndSend('hi')
  await act(async () => {
    ws.mockServerEvent({ type: 'text', text: 'Hello' })
  })
  await act(async () => {
    ws.mockServerEvent({ type: 'done', cost_usd: 0.5 })
  })

  expect(await screen.findByText('$0.50')).toBeOnTheScreen()
})

test('an error frame renders an error bubble', async () => {
  const { ws } = await connectMaster()
  typeAndSend('hi')
  await act(async () => {
    ws.mockServerEvent({ type: 'error', message: 'model overloaded' })
  })
  expect(await screen.findByText(/model overloaded/)).toBeOnTheScreen()
})

test('interrupting a turn emits an interrupt frame and shows the marker', async () => {
  const { ws } = await connectMaster()
  typeAndSend('long task')
  // Streaming → the stop control replaces send.
  await act(async () => {
    ws.mockServerEvent({ type: 'text', text: 'working' })
  })
  const stop = await screen.findByTestId('composer-stop')
  await act(async () => {
    fireEvent.press(stop)
  })
  expect(ws.frames().at(-1)).toEqual({ type: 'interrupt' })

  await act(async () => {
    ws.mockServerEvent({ type: 'interrupted', cost_usd: 0.01 })
  })
  expect(await screen.findByText(/interrupted/i)).toBeOnTheScreen()
})

test('replies to a server ping with a matching pong', async () => {
  const { ws } = await connectMaster()
  await act(async () => {
    ws.mockServerEvent({ type: 'ping', id: 42 })
  })
  expect(ws.frames()).toContainEqual({ type: 'pong', id: 42 })
})

test('a run of consecutive tool calls collapses into one group, expandable on tap', async () => {
  const { ws } = await connectMaster()
  typeAndSend('do a bunch of things')

  // Three tools stream back-to-back; only the first runs, the rest queue.
  await act(async () => {
    ws.mockServerEvent({ type: 'tool_use', tool_use_id: 't1', tool: 'edit_file', input: { path: 'a.ts' }, display: 'Editing file' })
    ws.mockServerEvent({ type: 'tool_use', tool_use_id: 't2', tool: 'bash',      input: { command: 'ls' }, display: 'Running command' })
    ws.mockServerEvent({ type: 'tool_use', tool_use_id: 't3', tool: 'read_file', input: { path: 'b.ts' }, display: 'Reading file' })
  })

  // Collapsed: the running step shows with an n/total counter; the queued
  // steps are hidden so the transcript isn't flooded.
  const runningLabel = await screen.findByText('Editing file (a.ts)')
  expect(runningLabel).toBeOnTheScreen()
  expect(screen.getByText('1/3')).toBeOnTheScreen()
  expect(screen.queryByText('Running command (ls)')).toBeNull()
  expect(screen.queryByText('Reading file (b.ts)')).toBeNull()

  // Tap the group header → expands to reveal every step.
  await act(async () => { fireEvent.press(runningLabel) })
  expect(await screen.findByText(/3 tool calls/)).toBeOnTheScreen()
  expect(screen.getByText('Editing file (a.ts)')).toBeOnTheScreen()
  expect(screen.getByText('Running command (ls)')).toBeOnTheScreen()
  expect(screen.getByText('Reading file (b.ts)')).toBeOnTheScreen()
})

test('a finished tool call from /history renders its friendly label, not the raw tool name', async () => {
  // /history carries a `display` phrase on tool rows; the client prefers it over
  // the raw `name(arg)` text so a reloaded/finished tool reads like it did live.
  onFetch('/history', () => ({
    messages: [
      { role: 'user', text: 'edit it' },
      { role: 'tool', text: 'edit_file(src/x.ts)', display: 'Editing file (src/x.ts)', output: 'ok' },
    ],
  }))
  await connectMaster()

  expect(await screen.findByText('Editing file (src/x.ts)')).toBeOnTheScreen()
  expect(screen.queryByText('edit_file(src/x.ts)')).toBeNull()
})

test('joining an in-flight multi-turn loop keeps completed turns when ready lands mid history-stagger', async () => {
  // Regression: a connect (or foreground return) that lands while the agent is
  // mid agentic-loop. /history carries every COMPLETED turn since the last user
  // message — each auto-turn fronted by the bg row that triggered it — while the
  // CURRENT in-flight turn is replayed over the socket.
  //
  // loadHistory reconciles that multi-row suffix with a staggered append (one
  // row per tick). If `ready { resumed: true }` arrives before the stagger has
  // drained, the replay-shadow anchor would snapshot a half-applied list, land
  // on the user message instead of the latest bg row, and `replay_end`'s swap
  // would wipe every completed turn in between — they'd vanish while the new
  // (current) turn shows. The fix folds the pending stagger queue into the
  // anchor first. Fake timers freeze the stagger so `ready` deterministically
  // lands mid-drain.
  jest.useFakeTimers()
  try {
    onFetch('/history', () => ({
      messages: [
        { role: 'user',        text: 'do the thing' },
        { role: 'assistant',   text: 'turn1 reply' },
        { role: 'bg_complete', text: 'taskA done' },
        { role: 'assistant',   text: 'turn2 reply' },
        { role: 'bg_complete', text: 'taskB done' },
      ],
    }))

    renderApp()
    fireEvent.changeText(screen.getByPlaceholderText('2:host:port:key'), VALID_CONNECT)
    await act(async () => { fireEvent.press(screen.getByText('connect')) })
    // Flush the noise-connect + /history promise chain (microtasks only) so the
    // /stream socket is constructed — WITHOUT advancing timers, so the stagger
    // ticker stays parked with the completed-turn rows still queued.
    for (let i = 0; i < 6; i++) await act(async () => {})

    const ws = lastWs()
    expect(ws).toBeTruthy()
    await act(async () => { ws.mockOpen() })

    // Server greets mid-turn, then replays the in-flight turn and seals it.
    await act(async () => {
      ws.mockServerEvent({ type: 'ready', session_id: 's1', resumed: true, model: 'sonnet' })
    })
    await act(async () => {
      ws.mockServerEvent({ type: 'text', text: 'turn3 reply' })
      ws.mockServerEvent({ type: 'replay_end' })
    })
    // Drain whatever timers remain (the fix clears the stagger; this just proves
    // no late re-append reorders or duplicates anything).
    await act(async () => { jest.runOnlyPendingTimers() })

    // All three assistant turns survive, in conversational order — the two
    // completed turns first, the freshly-replayed in-flight turn last.
    const order = screen
      .getAllByText(/^turn[123] reply$/)
      .map(n => n.props.children)
    expect(order).toEqual(['turn1 reply', 'turn2 reply', 'turn3 reply'])
  } finally {
    jest.useRealTimers()
  }
})

test('clearing the conversation POSTs /clear and empties the chat', async () => {
  // Echo the turn so the post-`done` reconcile keeps the bubbles on screen.
  onFetch('/history', () => ({
    messages: [
      { role: 'user', text: 'hello' },
      { role: 'assistant', text: 'hi there' },
    ],
  }))
  const { ws } = await connectMaster()
  typeAndSend('hello')
  await act(async () => {
    ws.mockServerEvent({ type: 'text', text: 'hi there' })
    ws.mockServerEvent({ type: 'done', cost_usd: 0 })
  })
  expect(await screen.findByText('hi there')).toBeOnTheScreen()

  await act(async () => {
    fireEvent.press(screen.getByText('clear'))
  })

  await waitFor(() =>
    expect(fetchCalls.some(c => c.url.endsWith('/clear') && c.init?.method === 'POST')).toBe(true),
  )
  expect(await screen.findByText('Awaiting Instructions')).toBeOnTheScreen()
})
