/**
 * Keyboard + chrome interactions that aren't tied to a specific server frame:
 * Enter-to-send (and Shift+Enter as a newline), and Disconnect returning to the
 * connect screen.
 */
import { resetAll, connectMaster, screen, fireEvent } from './helpers/render'

beforeEach(() => resetAll())

test('Enter sends the message', async () => {
  const { ws } = await connectMaster()
  const input = screen.getByTestId('composer-input')
  fireEvent.change(input, { target: { value: 'via enter' } })
  fireEvent.keyDown(input, { key: 'Enter' })
  expect(ws.frames().at(-1)).toEqual({ type: 'user_message', text: 'via enter' })
})

test('Shift+Enter does not send (newline)', async () => {
  const { ws } = await connectMaster()
  const input = screen.getByTestId('composer-input')
  fireEvent.change(input, { target: { value: 'line one' } })
  fireEvent.keyDown(input, { key: 'Enter', shiftKey: true })
  expect(ws.frames()).toEqual([])
})

test('Disconnect returns to the connect screen', async () => {
  await connectMaster()
  fireEvent.click(screen.getByText('Disconnect'))
  expect(await screen.findByTestId('qr-input')).toBeInTheDocument()
})
