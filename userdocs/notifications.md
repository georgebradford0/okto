# Push notifications

Push notifications are **on by default**. lair points at a small relay server,
and the mobile app registers its APNs/device token with it so background tools
(`send_notification`, `ask_question`) — and monitored [background
tasks](tasks.md) — can wake you when an agent has something to report. The relay
requires no sign-up.

## Opt out at init

```sh
okto init --disable-push
```

This persists `OKTO_RELAY_URL=` (an explicit empty value) into
`~/.okto/lair-env`, which:

1. **drops** the `send_notification` and `ask_question` tools from the LLM's tool
   list in both lair and child agents (so the model never offers to push), and
2. makes lair's `/info` advertise an empty relay URL, which the mobile client
   reads as "skip APNs registration entirely."

## Turn push back on later

No need to re-run `init` — just clear the override and reload:

```sh
okto env unset OKTO_RELAY_URL
okto reload
```

## Turn it off on an existing install

Set the empty value explicitly, then reload:

```sh
okto env set OKTO_RELAY_URL=
okto reload
```

!!! info "Even with the relay"
    A dedicated push tool exists, so you can always direct the model to notify
    you for any scenario you choose — it isn't limited to task completion.
