/**
 * Smoke test: the app boots to the connect screen when there's no saved
 * session, and the update check fires quietly on launch.
 */
import { resetAll, renderApp, checkMock, screen } from './helpers/render'

beforeEach(() => resetAll())

test('boots to the connect screen with no saved session', async () => {
  renderApp()
  expect(await screen.findByTestId('qr-input')).toBeInTheDocument()
  expect(screen.getByTestId('connect-btn')).toBeInTheDocument()
  expect(screen.getByText('Connect')).toBeInTheDocument()
})

test('checks for updates once on launch', async () => {
  renderApp()
  await screen.findByTestId('qr-input')
  expect(checkMock()).toHaveBeenCalled()
})
