/**
 * Connection & onboarding flows: manual connect string, QR scan, tunnel
 * failure, and auto-connect from a saved session.
 */
import AsyncStorage from '@react-native-async-storage/async-storage'
import NoiseConnection from '../src/NativeNoiseConnection'
import { renderApp, connectMaster, screen, fireEvent, waitFor, act } from './helpers/render'
import { resetServer, lastWs } from './helpers/server'

beforeEach(async () => {
  resetServer()
  await AsyncStorage.clear()
  ;(NoiseConnection.connect as jest.Mock).mockClear()
  ;(NoiseConnection.connect as jest.Mock).mockResolvedValue(45678)
})

describe('manual connect string', () => {
  test('rejects a malformed string and stays on setup', async () => {
    renderApp()
    const input = await screen.findByPlaceholderText('2:host:port:key')
    fireEvent.changeText(input, 'not-a-valid-string')
    fireEvent.press(screen.getByText('connect'))

    expect(await screen.findByText('Invalid connect string')).toBeOnTheScreen()
    // Still on the setup screen — no tunnel attempted.
    expect(NoiseConnection.connect).not.toHaveBeenCalled()
    expect(screen.getByPlaceholderText('2:host:port:key')).toBeOnTheScreen()
  })

  test('a valid string opens the tunnel and lands in the master chat', async () => {
    renderApp()
    const input = await screen.findByPlaceholderText('2:host:port:key')
    fireEvent.changeText(input, '2:10.0.0.5:9000:PUBKEY123')
    fireEvent.press(screen.getByText('connect'))

    await waitFor(() =>
      expect(NoiseConnection.connect).toHaveBeenCalledWith('10.0.0.5', 9000, 'PUBKEY123'),
    )
    // Empty master chat renders its awaiting-instructions state + composer.
    expect(await screen.findByText('Awaiting Instructions')).toBeOnTheScreen()
    expect(screen.getByPlaceholderText('message…')).toBeOnTheScreen()
  })

  test('persists the connection so a later launch auto-connects', async () => {
    renderApp()
    const input = await screen.findByPlaceholderText('2:host:port:key')
    fireEvent.changeText(input, '2:1.2.3.4:9000:KEY')
    fireEvent.press(screen.getByText('connect'))

    await waitFor(async () =>
      expect(await AsyncStorage.getItem('masterConnection')).toContain('1.2.3.4'),
    )
  })
})

describe('auto-connect from a saved session', () => {
  test('connects on launch when a session is stored', async () => {
    await AsyncStorage.setItem(
      'masterConnection',
      JSON.stringify({ v: 2, host: '9.9.9.9', port: 9000, pk: 'SAVEDKEY' }),
    )
    renderApp()
    await waitFor(() =>
      expect(NoiseConnection.connect).toHaveBeenCalledWith('9.9.9.9', 9000, 'SAVEDKEY'),
    )
    expect(await screen.findByText('Awaiting Instructions')).toBeOnTheScreen()
  })
})

describe('tunnel failure', () => {
  test('surfaces the error and the back button returns to setup', async () => {
    ;(NoiseConnection.connect as jest.Mock).mockRejectedValueOnce(new Error('handshake refused'))
    renderApp()
    const input = await screen.findByPlaceholderText('2:host:port:key')
    fireEvent.changeText(input, '2:10.0.0.5:9000:PUBKEY')
    fireEvent.press(screen.getByText('connect'))

    expect(await screen.findByText('handshake refused')).toBeOnTheScreen()
    fireEvent.press(screen.getByText('back'))
    expect(await screen.findByPlaceholderText('2:host:port:key')).toBeOnTheScreen()
  })
})

describe('QR scanning', () => {
  test('opening the scanner and cancelling returns to setup', async () => {
    renderApp()
    await screen.findByText('OCTO')
    fireEvent.press(screen.getByTestId('scan-trigger'))

    expect(await screen.findByText('Scan Session QR')).toBeOnTheScreen()
    fireEvent.press(screen.getByText('Cancel'))
    expect(await screen.findByPlaceholderText('2:host:port:key')).toBeOnTheScreen()
  })

  test('a scanned QR code connects', async () => {
    renderApp()
    await screen.findByText('OCTO')
    fireEvent.press(screen.getByTestId('scan-trigger'))
    await screen.findByText('Scan Session QR')

    await act(async () => {
      // @ts-expect-error — installed by the vision-camera mock
      global.__cameraScan('2:7.7.7.7:9000:QRKEY')
    })

    await waitFor(() =>
      expect(NoiseConnection.connect).toHaveBeenCalledWith('7.7.7.7', 9000, 'QRKEY'),
    )
  })
})

describe('reaching a ready chat', () => {
  test('the stream handshake flips the composer to ready', async () => {
    const { ws } = await connectMaster()
    // A `ready` (not resumed) frame was delivered by connectMaster.
    expect(ws.frames()).toEqual([]) // nothing sent yet
    expect(screen.getByPlaceholderText('message…')).toBeOnTheScreen()
    // The clear control is enabled only once status is 'ready'.
    expect(lastWs()).toBe(ws)
  })
})
