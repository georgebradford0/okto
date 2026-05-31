// Stable selectors for the desktop behavioural suite (desktop/__tests__/**).
//
// These are the `data-testid` values wired into App.tsx's interactive surfaces.
// Keeping them in one place lets the tests reference symbols instead of magic
// strings, and documents the testable surface in one glance. Mirrors
// mobile/src/testIds.ts where the two UIs overlap.
//
// Per-entity rows append a runtime id, so they're exposed as helper functions.

export const TestIds = {
  // ── Connect screen ──────────────────────────────────────────────────────────
  qrInput: 'qr-input',
  connectBtn: 'connect-btn',

  // ── Composer ────────────────────────────────────────────────────────────────
  composerInput: 'composer-input',
  composerSend: 'composer-send',
  composerStop: 'composer-stop',

  // ── Background tasks ────────────────────────────────────────────────────────
  tasksButton: 'tasks-button',

  // ── Per-entity rows (id-suffixed) ───────────────────────────────────────────
  sidebarRow: (id: string) => `sidebar-row-${id}`,
  taskRow: (id: string) => `task-row-${id}`,
  taskCancel: (id: string) => `task-cancel-${id}`,
  // A tool chip in the chat scroll; `id` is the wire tool_use_id.
  toolRow: (id: string) => `tool-${id}`,
} as const
