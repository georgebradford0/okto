/**
 * Core chat behaviour over the /stream socket: sending a turn, streaming
 * assistant text and tool calls, turn terminators (done / error / interrupted),
 * keepalive, and clearing the conversation.
 */
import { resetAll, connectMaster, screen, fireEvent, act } from './helpers/render'
import { onFetch, fetchCalls } from './helpers/server'

beforeEach(() => resetAll())

// NB: don't wrap change+click in one act — React would batch them so the send
// handler's closure still sees the pre-change (empty) input. Each fireEvent
// self-wraps in act and flushes the re-render between the two events.
function typeAndSend(text: string) {
  fireEvent.change(screen.getByTestId('composer-input'), { target: { value: text } })
  fireEvent.click(screen.getByTestId('composer-send'))
}

test('sending a message echoes a user bubble and emits a user_message frame', async () => {
  const { ws } = await connectMaster()
  typeAndSend('build me a feature')

  expect(await screen.findByText('build me a feature')).toBeInTheDocument()
  expect(ws.frames().at(-1)).toEqual({ type: 'user_message', text: 'build me a feature' })
})

test('empty input does not send', async () => {
  const { ws } = await connectMaster()
  fireEvent.change(screen.getByTestId('composer-input'), { target: { value: '   ' } })
  fireEvent.click(screen.getByTestId('composer-send'))
  expect(ws.frames()).toEqual([])
})

test('streams assistant text deltas into a single bubble', async () => {
  const { ws } = await connectMaster()
  typeAndSend('hi')

  await act(async () => {
    ws.mockServerEvent({ type: 'text', text: 'Hello' })
    ws.mockServerEvent({ type: 'text', text: ', world' })
  })

  expect(await screen.findByText('Hello, world')).toBeInTheDocument()
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

  // The chip carries the label `display (firstArg)`; under react-native-web that
  // string is split across text nodes, so we target the chip by its stable
  // testID (the wire tool_use_id) rather than by text. Output is collapsed
  // until the chip is clicked.
  const toolRow = await screen.findByTestId('tool-tu1')
  expect(toolRow).toHaveTextContent('bash (ls -la)')
  expect(screen.queryByText(/file-a\.txt/)).toBeNull()

  await act(async () => {
    fireEvent.click(toolRow)
  })
  expect(await screen.findByText(/file-a\.txt/)).toBeInTheDocument()

  // A tool_result supersedes the streamed lines with the final output.
  await act(async () => {
    ws.mockServerEvent({ type: 'tool_result', tool_use_id: 'tu1', output: 'exit 0' })
  })
  expect(await screen.findByText(/exit 0/)).toBeInTheDocument()
})

test('a done frame seals the turn and returns the composer to ready', async () => {
  // History echoes the turn so the end-of-turn reconcile keeps the bubble.
  onFetch('/history', () => ({
    messages: [
      { role: 'user', text: 'hi' },
      { role: 'assistant', text: 'Hello' },
    ],
  }))
  const { ws } = await connectMaster()
  typeAndSend('hi')

  // Mid-stream the stop control is shown (streaming).
  await act(async () => {
    ws.mockServerEvent({ type: 'text', text: 'Hello' })
  })
  expect(await screen.findByTestId('composer-stop')).toBeInTheDocument()

  // `done` seals the turn: the assistant text stays and the composer flips back
  // from the stop control to the send button (ready).
  await act(async () => {
    ws.mockServerEvent({ type: 'done', cost_usd: 0.5 })
  })
  expect(await screen.findByTestId('composer-send')).toBeInTheDocument()
  expect(screen.queryByTestId('composer-stop')).toBeNull()
  expect(screen.getByText('Hello')).toBeInTheDocument()
})

test('an error frame renders an error bubble', async () => {
  const { ws } = await connectMaster()
  typeAndSend('hi')
  await act(async () => {
    ws.mockServerEvent({ type: 'error', message: 'model overloaded' })
  })
  expect(await screen.findByText(/model overloaded/)).toBeInTheDocument()
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
    fireEvent.click(stop)
  })
  expect(ws.frames().at(-1)).toEqual({ type: 'interrupt' })

  await act(async () => {
    ws.mockServerEvent({ type: 'interrupted', cost_usd: 0.01 })
  })
  expect(await screen.findByText(/Interrupted/i)).toBeInTheDocument()
})

test('replies to a server ping with a matching pong', async () => {
  const { ws } = await connectMaster()
  await act(async () => {
    ws.mockServerEvent({ type: 'ping', id: 42 })
  })
  expect(ws.frames()).toContainEqual({ type: 'pong', id: 42 })
})

test('clearing the conversation POSTs /clear and empties the chat', async () => {
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
  expect(await screen.findByText('hi there')).toBeInTheDocument()

  await act(async () => {
    fireEvent.click(screen.getByText('Clear'))
  })

  expect(
    fetchCalls.some(c => c.url.endsWith('/clear') && c.init?.method === 'POST'),
  ).toBe(true)
  expect(await screen.findByText('Awaiting your first message')).toBeInTheDocument()
})
