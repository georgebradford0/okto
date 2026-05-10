// Wire schema for the /stream WebSocket between mobile and lair/server.
//
// Mirrors octo_core::ChatEvent in core/src/lib.rs (Rust) plus the small set of
// client → server frames. Update this file whenever the Rust enum or the
// stream-handler JSON shape changes; both sides MUST stay in sync.
//
// JSON tagging: `{"type": "<snake_case>", ...fields}` (matches serde tag="type").

// ── Server → client events ────────────────────────────────────────────────────

export type ServerEvent =
  | { type: 'ready';         session_id: string; resumed: boolean }
  | { type: 'text';          text: string }
  | { type: 'tool_use';      tool: string; input: unknown; display?: string }
  | { type: 'tool_output';   line: string }
  // NB: wire field is `output` (hand-coded in lair/server), not `content` as
  // the auto-derived ChatEvent::ToolResult would produce. Keep this aligned.
  | { type: 'tool_result';   tool_use_id: string; output: unknown }
  | { type: 'done';          cost_usd: number }
  | { type: 'error';         message: string }
  | { type: 'interrupted';   cost_usd: number }
  | { type: 'interrupt_ack' }
  | { type: 'system';        text: string }
  | { type: 'containers';    containers: ContainerInfo[] }
  | { type: 'tasks';         tasks: TaskRecord[] }
  | { type: 'bg_complete';   task_id: string; text: string }
  | { type: 'ping';          id: number }
  | { type: 'pong';          id: number }

// ── Client → server frames ────────────────────────────────────────────────────

export type ClientFrame =
  | { type: 'user_message';    text: string }
  | { type: 'interrupt' }
  | { type: 'ping';            id: number }
  | { type: 'pong';            id: number }
  | { type: 'start_container'; id: string }
  | { type: 'terminate_agent'; id: string }
  | { type: 'cancel_task';     id: string }
  // Legacy: child server's "watch" mode and the original first-frame `{text}`
  // shape are still accepted server-side; remove these when the persistent
  // /stream rewrite lands.
  | { type: 'watch' }
  | { text: string }

// ── Shared payloads ───────────────────────────────────────────────────────────

export interface ContainerInfo {
  id:      string
  name:    string
  git_url: string
  status:  string
  host:    string
  port:    number
  pubkey:  string
}

/** Mirrors octo_core::TaskRecord. The per-chat background-task registry is
 *  pushed as a `tasks` event on /stream open and after every spawn / completion.
 *  `started_at` and `completed_at` are Unix-epoch seconds. */
export interface TaskRecord {
  task_id:          string
  task_description: string
  status:           'running' | 'done' | 'error' | 'cancelled'
  started_at:       number
  completed_at:     number | null
  summary:          string | null
  cost_usd:         number | null
}

// ── Helpers ───────────────────────────────────────────────────────────────────

export function parseServerEvent(raw: string): ServerEvent | null {
  try {
    const v = JSON.parse(raw) as ServerEvent
    return typeof v === 'object' && v !== null && typeof v.type === 'string' ? v : null
  } catch {
    return null
  }
}

export function encodeClientFrame(frame: ClientFrame): string {
  return JSON.stringify(frame)
}
