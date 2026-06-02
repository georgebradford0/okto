# Background tasks

Agents can run long-lived work in the background — a build, a test watch, a
training run — and (optionally) attach a **monitor** that wakes the model with
new output on an interval. The CLI lets you see and stop these tasks from the
host.

## List

```sh
okto tasks list                  # aggregate across lair + all known agents
okto tasks list --agent lair-myrepo   # just one agent
```

Columns: **task id**, **agent**, **status** (`running` / `done` / `error` /
`cancelled`), **started**, and the **command** (first line, truncated). Task
state is read from disk:

- lair: `~/.okto/lair/session/tasks.json`
- agent: `~/.okto/agents/<name>/data/session/tasks.json`

## Stop

```sh
okto tasks stop <id>                    # a lair-local task
okto tasks stop --agent lair-myrepo <id>   # an agent-local task
```

The response reports `fired: true` if the task was actually running and has now
been cancelled, or `fired: false` if it was already finished or the id doesn't
exist.

!!! tip "Tasks vs. notifications"
    A monitored background task can wake you with [push
    notifications](notifications.md) when it produces output or completes — handy
    for long runs you don't want to babysit in the chat.
