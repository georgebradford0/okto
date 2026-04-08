# Server-authoritative message protocol

## Problem

The mobile client was maintaining its own message list in `AsyncStorage` and
reconciling it with the server's `history` frame on every reconnect via
`mergeSessionBubbles`.  This merge function had to handle:

- index-based `h${i}` IDs colliding across reconnects (FlatList key duplication)
- in-progress session bubbles being inserted by the merge *and* re-created by
  the next `session_start` frame (visual duplicate session logs)
- client-only bubbles (session summaries, tool lines) that never appear in
  server history needing to be threaded back in

The result was ~100 lines of fragile reconciliation logic and two recurring
bugs even after targeted patches.

## Root cause

The client was trying to be the source of truth for UI state that the server
already owns.  The server has the complete authoritative record:

- **Completed turns** — persisted in `session/messages.json`, replayed as
  `history` on connect.
- **In-progress turn** — buffered in `LiveBuffer`, replayed event-by-event
  from the beginning on every reconnect.

## Solution

Assign stable, server-issued sequence numbers to every item the server sends,
then have the client replace its state unconditionally from the server's replay.
No merge, no AsyncStorage message cache, no client-side IDs.

### Server changes

1. **`HistMsg`** gains a `seq: usize` field — a monotonically incrementing
   counter assigned once when a message is first appended and persisted with it.

2. **Every live `WsFrame`** variant gains a `seq: usize` field — the event's
   index in `LiveBuffer.events` (0-based, reset on each new gen).

3. The existing `live_gen` field is kept so the client can ignore frames from a
   stale generation.

### Mobile changes

1. **Drop `mergeSessionBubbles`** and all AsyncStorage message caching.  The
   only local state worth persisting is the single unsent pending message string
   (for the optimistic user bubble before `ack`).

2. **On `history`**: replace the message list directly from `frame.messages`,
   keyed by `h${seq}`.

3. **On live events**: upsert into a parallel live-frames map keyed by
   `${live_gen}_${seq}`.  The rendered list is `history messages` +
   `live frames` in seq order.  Because the key is stable, replaying the same
   events on reconnect is naturally idempotent — no duplicate bubbles.

4. **Event mapping** (same as before, now keyed by seq):
   - `session_start` → create session bubble at `seq`
   - `token` → append text into the open session bubble (or bare assistant
     bubble if no session is open)
   - `tool` → append tool line into the open session bubble
   - `session_end` → close session bubble, attach summary text
   - `done` / `error` → close current response
   - `question` → create question bubble

5. **Optimistic user bubble**: still generated client-side on send, replaced by
   the `history` entry after `ack` + reconnect.  No change needed here.

## Properties

| Property | Before | After |
|---|---|---|
| Source of truth | Client AsyncStorage + server | Server only |
| Reconnect logic | ~100-line merge function | Replace state from replay |
| Duplicate-message risk | Yes (ID collisions, double-insert) | No (stable server seq) |
| Session log duplication | Yes (merge + session_start) | No (upsert by seq) |
| AsyncStorage usage | Full message list cached | Pending message string only |
