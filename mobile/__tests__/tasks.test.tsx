/**
 * Background-tasks UI: the header badge running-count, the tasks modal, and the
 * optimistic STOP / cancel-guard (cancel_task → STOPPING → released on ack).
 */
import AsyncStorage from '@react-native-async-storage/async-storage'
import NoiseConnection from '../src/NativeNoiseConnection'
import { connectMaster, screen, fireEvent, act } from './helpers/render'
import { resetServer } from './helpers/server'

beforeEach(async () => {
  resetServer()
  await AsyncStorage.clear()
  ;(NoiseConnection.connect as jest.Mock).mockClear()
  ;(NoiseConnection.connect as jest.Mock).mockResolvedValue(45678)
})

const nowSecs = () => Math.floor(Date.now() / 1000)

function task(over: Partial<any> = {}) {
  return {
    task_id: 't1',
    command: 'compile the project',
    status: 'running',
    started_at: nowSecs() - 5,
    completed_at: null,
    summary: null,
    cost_usd: null,
    ...over,
  }
}

async function pushTasks(ws: any, tasks: any[]) {
  await act(async () => {
    ws.mockServerEvent({ type: 'tasks', tasks })
  })
}

test('the header badge reflects the running task count', async () => {
  const { ws } = await connectMaster()
  expect(screen.getByText('TASKS')).toBeOnTheScreen()

  await pushTasks(ws, [task(), task({ task_id: 't2', status: 'done', completed_at: nowSecs() })])
  expect(await screen.findByText('TASKS · 1')).toBeOnTheScreen()
})

test('the modal lists tasks and an empty state when there are none', async () => {
  const { ws } = await connectMaster()

  await act(async () => {
    fireEvent.press(screen.getByText('TASKS'))
  })
  expect(await screen.findByText('Background Tasks')).toBeOnTheScreen()
  expect(screen.getByText('No background tasks')).toBeOnTheScreen()

  await pushTasks(ws, [task()])
  expect(await screen.findByText('compile the project')).toBeOnTheScreen()
  expect(screen.getByText('RUNNING')).toBeOnTheScreen()
})

test('STOP optimistically cancels and releases on a no-op ack', async () => {
  const { ws } = await connectMaster()
  await pushTasks(ws, [task()])

  await act(async () => {
    fireEvent.press(screen.getByText('TASKS · 1'))
  })
  const stop = await screen.findByText('STOP')

  await act(async () => {
    fireEvent.press(stop)
  })
  // A cancel_task frame went out and the button latches to STOPPING.
  expect(ws.frames()).toContainEqual({ type: 'cancel_task', id: 't1' })
  expect(await screen.findByText('STOPPING')).toBeOnTheScreen()

  // Server had nothing live to kill → the latch releases back to STOP.
  await act(async () => {
    ws.mockServerEvent({ type: 'cancel_task_ack', id: 't1', fired: false })
  })
  expect(await screen.findByText('STOP')).toBeOnTheScreen()
})

test('a tasks update moving the task off running clears the STOPPING latch', async () => {
  const { ws } = await connectMaster()
  await pushTasks(ws, [task()])
  await act(async () => {
    fireEvent.press(screen.getByText('TASKS · 1'))
  })
  await act(async () => {
    fireEvent.press(await screen.findByText('STOP'))
  })
  expect(await screen.findByText('STOPPING')).toBeOnTheScreen()

  // The authoritative registry now reports the task as cancelled.
  await pushTasks(ws, [task({ status: 'cancelled', completed_at: nowSecs() })])
  expect(await screen.findByText('CANCELLED')).toBeOnTheScreen()
  expect(screen.queryByText('STOPPING')).toBeNull()
  expect(screen.queryByText('STOP')).toBeNull()
})
