# Background tasks & process monitoring

Two tools let the model run work outside the current chat turn:

- **`run_command_in_background(command)`** — fire-and-forget. The model is told the
  result *once, when the command finishes*.
- **`monitor_process(...)`** — the model is woken with the process's output *while it
  runs*, so it can react mid-run (a failing build, a milestone, an error in a log).

Both are available to lair and to every child agent. As of `lair 0.11.6`.

## When to use which

| | `run_command_in_background` | `monitor_process` |
|---|---|---|
| Model sees output | once, on completion | repeatedly, while running + on completion |
| Triggers a model turn | once (`bg_complete`) | every wake-up (`bg_progress`) + once (`bg_complete`) |
| Good for | long builds, big test suites, large downloads | watching a log, a slow build you want to react to, a streaming process |
| Cadence | n/a | model-chosen `wake_interval_secs` (default 60 s, floor 15 s) |
| Cost | one extra turn | one turn per wake-up with new output — **no cap** |

Rule of thumb: use `run_command_in_background` unless the model genuinely needs to act
*before* the process ends. A monitor that watches a chatty hours-long process keeps
waking the model the whole time.

## The shape

Everything is a **background task** — a `bash -c` child process with a registry row
(`TaskRecord`) and a bounded live output buffer (`TaskOutput`). A task *may* have a
**monitor** attached: a detached loop that periodically wakes the model with the
task's new output.

```
run_command_in_background(command)   → task                    → bg_complete
monitor_process(command, interval)   → task + monitor           → bg_progress… + bg_complete
monitor_process(task_id, interval)   → monitor on existing task → bg_progress… + bg_complete
```

`monitor_process` takes **either** `command` (start and watch a fresh process) **or**
`task_id` (attach to a task already started by `run_command_in_background`), plus an
optional `wake_interval_secs` and `purpose`. "Remote" monitoring is just the command
reaching out — e.g. `ssh host 'journalctl -f'`; the tool has no special remote mode.

## Lifecycle

### A plain background task

1. `exec_run_command_in_background` mints a `task_id`, registers a `Running`
   `TaskRecord`, and `register_task` returns a shared `TaskOutput` buffer.
2. `spawn_background_command` runs `bash -c <command>` detached, appending every
   stdout/stderr line to the buffer as it arrives.
3. On exit (success, failure, or cancellation) the deliver closure finalizes the
   registry row, fans out a `bg_complete` event, queues a `bg_complete` injection,
   fires a push notification, and calls `try_continue_auto`.

### A monitored task

Same as above, plus `spawn_monitor` starts a detached loop that, every
`wake_interval_secs`:

1. reads new output from the `TaskOutput` buffer since its last cursor;
2. if there's new output, queues a `bg_progress` injection, fans out a `bg_progress`
   event, and calls `try_continue_auto`;
3. exits once the task leaves `Running` or its cancel token fires.

A wake-up with no new output is skipped — the model is not woken for nothing.

## The injection queue

A background task can finish — or a monitor can wake — at any moment, including
*during* an in-flight turn. A turn snapshots the message log at its start and commits
the evolved log at its end, so anything appended to `messages` mid-turn would be
clobbered by that commit.

To avoid this, `bg_complete` and `bg_progress` injections are **never written to
`messages` directly**. They are staged in `AppState.pending_injections`.
`try_continue_auto`:

- no-ops if the queue is empty;
- otherwise claims the `is_streaming` slot — if a turn is already running it bails,
  and that turn's own end-of-turn `try_continue_auto` drains the queue instead;
- drains the queue into `messages`, then spawns an auto-turn so the model reacts.

So injections only ever reach `messages` while no turn is running. This also fixed a
latent bug where a `bg_complete` landing mid-turn could be lost.

## What the model sees

`bg_complete` / `bg_progress` are persisted-only `ApiMessage` roles. `compact_history`
rewrites them to `user` before the API call (the Anthropic API merges consecutive
same-role messages, so back-to-back injections are fine). Each wake-up prompt asks the
model to act only if the output warrants it, otherwise to acknowledge briefly.

## Wire & mobile

| Event | Direction | Meaning |
|---|---|---|
| `bg_complete` | s → c | a task finished; mobile renders a `◇` chip |
| `bg_progress` | s → c | a monitored task produced new output; mobile renders a `◈` chip |

`TaskRecord.wake_interval_secs` is sent on the `tasks` frame; the Tasks modal shows a
`◈ MONITORED` badge for monitored tasks. The `bg_progress` event text is identical to
the persisted message text, so a `/history` reload reconciles cleanly.

## Cancellation

`cancel_task` fires the task's `CancellationToken`: the `bash` child is SIGKILLed and
any attached monitor loop stops. Note `child.kill()` only kills the direct `bash`
process — grandchild processes the command spawned are not reaped.

## Implementation

```
core/src/background.rs
  TaskOutput                       — bounded live output buffer (64 KB tail)
  TaskRecord.wake_interval_secs    — Some(n) ⇒ monitored
  spawn_background_command         — runs bash -c, appends output to the buffer
  register_task                    — registers row + buffer, returns the buffer handle
  run_command_in_background_tool / monitor_process_tool — tool specs
  monitor_progress_text / _message — bg_progress wake prompt
core/src/app.rs
  StreamState.task_outputs         — task_id → shared TaskOutput
core/src/lib.rs
  ChatEvent::BgProgress            — wire event
  compact_history                  — bg_complete/bg_progress → user for the API
lair/src/agent.rs, lair/src/lair.rs   (duplicated per role)
  AppState.pending_injections      — staged injections
  try_continue_auto                — drains the queue, spawns an auto-turn
  run_tracked_command              — spawn + standard completion handling
  exec_monitor_process / spawn_monitor — the monitor tool + its loop
```

## Limits & non-goals

- **Follow-mode only.** A monitor runs its command once and streams until it exits or
  is cancelled. Re-running a status command on a timer (poll mode) is not supported.
- **No cost cap.** A monitor keeps waking the model as long as the process produces
  output; the model picks the cadence per process.
- **No detach.** Cancelling a monitored task stops the process and the monitor
  together; there's no way to stop only the monitor.
