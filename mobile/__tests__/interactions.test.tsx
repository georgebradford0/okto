/**
 * Assorted whole-app flows: replaying server history on connect, the @-file
 * completion popup, the connection-lost modal on a send with a dead socket, and
 * logout returning to the setup screen.
 */
import AsyncStorage from '@react-native-async-storage/async-storage'
import NoiseConnection from '../src/NativeNoiseConnection'
import { connectMaster, screen, fireEvent, waitFor, act } from './helpers/render'
import { resetServer, onFetch } from './helpers/server'

beforeEach(async () => {
  resetServer()
  await AsyncStorage.clear()
  ;(NoiseConnection.connect as jest.Mock).mockClear()
  ;(NoiseConnection.connect as jest.Mock).mockResolvedValue(45678)
})

test('replays existing server history when the chat connects', async () => {
  onFetch('/history', () => ({
    messages: [
      { role: 'user', text: 'previous question' },
      { role: 'assistant', text: 'previous answer' },
    ],
  }))
  await connectMaster()

  expect(await screen.findByText('previous question')).toBeOnTheScreen()
  expect(await screen.findByText('previous answer')).toBeOnTheScreen()
})

test('renders markdown — a fenced code block — in an assistant message', async () => {
  const { ws } = await connectMaster()
  await act(async () => {
    ws.mockServerEvent({ type: 'text', text: 'Here:\n```js\nconst x = 1\n```' })
  })
  expect(await screen.findByText('const x = 1')).toBeOnTheScreen()
  // The language tag is surfaced as its own label.
  expect(screen.getByText('js')).toBeOnTheScreen()
})

test('the @ completion popup fetches and inserts a path', async () => {
  onFetch('/completions', () => ['src/components/'])
  await connectMaster()

  fireEvent.changeText(screen.getByPlaceholderText('message…'), '@src/comp')

  const suggestion = await screen.findByText('src/components/')
  await act(async () => {
    fireEvent.press(suggestion)
  })
  expect(screen.getByDisplayValue('@src/components/')).toBeOnTheScreen()
})

test('sending on a dead socket surfaces the connection-lost modal', async () => {
  const { ws } = await connectMaster()
  // The tunnel drops; the live socket is gone.
  await act(async () => {
    ws.mockDrop()
  })

  fireEvent.changeText(screen.getByPlaceholderText('message…'), 'are you there?')
  fireEvent.press(screen.getByTestId('composer-send'))

  expect(await screen.findByText(/network error/)).toBeOnTheScreen()
  expect(await screen.findByText('Connection Lost')).toBeOnTheScreen()

  await act(async () => {
    fireEvent.press(screen.getByText('Dismiss'))
  })
  await waitFor(() => expect(screen.queryByText('Connection Lost')).toBeNull())
})

test('logout wipes the session and returns to the setup screen', async () => {
  await connectMaster()
  await act(async () => {
    fireEvent.press(screen.getByTestId('open-sidebar'))
  })
  await act(async () => {
    fireEvent.press(screen.getByText('exit'))
  })

  expect(await screen.findByPlaceholderText('2:host:port:key')).toBeOnTheScreen()
  expect(await AsyncStorage.getItem('masterConnection')).toBeNull()
})
