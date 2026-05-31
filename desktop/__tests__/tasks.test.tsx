/**
 * Background-task registry: lair pushes a `tasks` frame on every spawn /
 * completion / cancellation; the tasks button reflects the running count, the
 * drawer lists them, and the Stop control emits a `cancel_task` frame.
 */
import { resetAll, connectMaster, screen, fireEvent, act } from './helpers/render'

beforeEach(() => resetAll())

const task = (over = {}) => ({
  task_id: 't1',
  command: 'long build',
  status: 'running',
  started_at: 1_700_000_000,
  completed_at: null,
  summary: null,
  cost_usd: null,
  ...over,
})

test('a tasks frame surfaces the running count on the tasks button', async () => {
  const { ws } = await connectMaster()
  await act(async () => {
    ws.mockServerEvent({ type: 'tasks', tasks: [task()] })
  })
  expect(await screen.findByText(/Tasks · 1/)).toBeInTheDocument()
})

test('opening the drawer lists tasks and cancelling emits a cancel_task frame', async () => {
  const { ws } = await connectMaster()
  await act(async () => {
    ws.mockServerEvent({ type: 'tasks', tasks: [task()] })
  })

  fireEvent.click(await screen.findByTestId('tasks-button'))
  expect(await screen.findByTestId('task-row-t1')).toBeInTheDocument()
  expect(screen.getByText('long build')).toBeInTheDocument()

  fireEvent.click(screen.getByTestId('task-cancel-t1'))
  expect(ws.frames().at(-1)).toEqual({ type: 'cancel_task', id: 't1' })
})

test('a completed task drops the running count back to plain "Tasks"', async () => {
  const { ws } = await connectMaster()
  await act(async () => {
    ws.mockServerEvent({ type: 'tasks', tasks: [task()] })
  })
  await screen.findByText(/Tasks · 1/)

  await act(async () => {
    ws.mockServerEvent({ type: 'tasks', tasks: [task({ status: 'done', completed_at: 1_700_000_100 })] })
  })
  expect(await screen.findByText('Tasks')).toBeInTheDocument()
})
