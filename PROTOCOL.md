# Wire protocol versioning

The clients (`mobile/`, `desktop/`) and the backend (`lair/`, and child agents)
talk over a small wire: the `/stream` WebSocket `ChatEvent` frames plus the
`/info` and `/history` JSON shapes (see `core/src/lib.rs::ChatEvent`,
`mobile/src/wire.ts`, `desktop/src/wire.ts`).

To align breaking changes across artifacts that release on **independent
cadences** (the lair Docker image, the iOS app behind App Store review, the
Android track, the desktop DMG), the wire carries a single integer version that
is **separate from every app's marketing version**.

## The version

- **Source of truth:** `okto_core::WIRE_PROTOCOL` (`core/src/lib.rs`).
- **Mirrored** (and asserted equal by CI) in `mobile/src/wire.ts` and
  `desktop/src/wire.ts` as `WIRE_PROTOCOL` — the version each client build
  speaks. A jest test in each client (`__tests__/wireProtocol.test.tsx`) reads
  the Rust constant and fails the build on drift.
- **Advertised by lair** on `GET /info` (`"wire_protocol"`) and in every
  `ready` WebSocket frame, alongside `"lair_version"` (the marketing version,
  for display/support only — never for compatibility decisions).

## Compatibility model

**lair is the compatibility bearer: it stays backward-compatible across all
client versions and never rejects a client on version.** A client connects,
reads the lair's advertised `wire_protocol`, and compares it to its own
`WIRE_PROTOCOL`:

- equal → no notice.
- lair **older** than the app → "update your okto host (`okto lair update`)".
- lair **newer** than the app → "update the app".

The notice is advisory (a banner), not a hard block — because changes are
expected to be backward-compatible (see below), the mismatch is a heads-up, not
a failure.

## When to bump `WIRE_PROTOCOL`

| Change to the wire | Bump? |
|--------------------|-------|
| New **optional** field on an existing event / `/info` / `/history` | **No** — old side ignores it. |
| New event `type`, new endpoint | **No** — old side never sends/relies on it. |
| **Remove or rename** a field; change the **meaning** of an existing field | **Yes.** |
| Make a previously-optional field **required**, or require a **new event** to function | **Yes.** |

Prefer additive, backward-compatible changes (e.g. the `display` field added to
`/history` tool rows in protocol 1 — optional, old clients ignore it). Reserve
bumps for genuinely breaking changes, and keep lair able to serve older clients.

**To bump:** change `okto_core::WIRE_PROTOCOL`, update both `wire.ts` mirrors to
match (CI enforces this), add a row to the history below, and note it in the
relevant `CHANGELOG.md`s.

## History

| Protocol | Introduced | Notes |
|----------|-----------|-------|
| **1** | lair 0.21.6 / mobile 0.2.2 / desktop 0.4.6 | Baseline. lair advertises `wire_protocol` + `lair_version` on `/info` and in `ready`; clients mirror `WIRE_PROTOCOL` and reference the lair's. A lair predating this (≤ 0.21.5) sends no `wire_protocol`, which clients treat as `0` (legacy). The additive `display` field on `/history` tool rows (shipped in lair 0.21.5) needed no bump — it was backward-compatible. |
| **2** | lair 0.21.7 / mobile 0.2.3 / desktop 0.4.7 | The `agents` event's `id` is now a **route-safe slug** that may differ from the free-form `name` (previously `id === name`). Clients must build agent proxy URLs (`/agents/<id>/…`) from `id`, not `name`, and `parent` now references a parent's `id` (slug). Bumped because the meaning of `id` changed — a client that routed by `name` (older mobile) breaks against an agent whose name isn't already slug-shaped. |
