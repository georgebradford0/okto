/**
 * Core chat behaviour over the /stream socket: sending a turn, streaming
 * assistant text and tool calls, turn terminators (done / error / interrupted),
 * keepalive, and clearing the conversation.
 */
import AsyncStorage from '@react-native-async-storage/async-storage'
import NoiseConnection from '../src/NativeNoiseConnection'
import { connectMaster, screen, fireEvent, waitFor, act } from './helpers/render'
import { resetServer, onFetch, fetchCalls } from './helpers/server'

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
