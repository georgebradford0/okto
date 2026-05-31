/**
 * Smoke test — the app boots to the setup screen with no saved connection.
 * @format
 */
import { renderApp, screen } from './helpers/render'
import { resetServer } from './helpers/server'

beforeEach(() => resetServer())

test('renders the setup screen on a cold start', async () => {
  renderApp()
  expect(await screen.findByText('OCTO')).toBeOnTheScreen()
  expect(screen.getByText(/Tap the mark to scan/)).toBeOnTheScreen()
  expect(screen.getByPlaceholderText('2:host:port:key')).toBeOnTheScreen()
})
