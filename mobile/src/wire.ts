// Wire schema for the /stream WebSocket between mobile and lair/agent.
//
// Mirrors okto_core::ChatEvent in core/src/lib.rs (Rust) plus the small set of
// client → server frames. Update this file whenever the Rust enum or the
// stream-handler JSON shape changes; both sides MUST stay in sync.
//
// JSON tagging: `{"type": "<snake_case>", ...fields}` (matches serde tag="type").

// ── Wire protocol version ─────────────────────────────────────────────────────

// Revision of the wire protocol this client was built against. MUST stay in
// lockstep with `okto_core::WIRE_PROTOCOL` (core/src/lib.rs) — a CI check
// (`__tests__/wireProtocol.test.tsx`) fails the build if they diverge. lair
// advertises its own version on `/info` and in every `ready` frame; we compare
// the two to warn the user when one side is behind (see `App.tsx`). Bump only
// on a breaking wire change — see PROTOCOL.md / CLAUDE.md.
export const WIRE_PROTOCOL = 1

// ── Server → client events ────────────────────────────────────────────────────

export type ServerEvent =
  // `wire_protocol` is optional: lairs older than protocol 1 don't send it.
  | { type: 'ready';         session_id: string; resumed: boolean; model: string; wire_protocol?: number }
  | { type: 'replay_end' }
  | { type: 'text';          text: string }
  | { type: 'tool_use';      tool_use_id: string; tool: string; input: unknown; display?: string }
  | { type: 'tool_output';   tool_use_id: string; line: string }
  | { type: 'tool_result';   tool_use_id: string; output: unknown }
  | { type: 'done';          cost_usd: number }
  | { type: 'error';         message: string }
  | { type: 'interrupted';   cost_usd: number }
  | { type: 'interrupt_ack' }
  | { type: 'cancel_task_ack'; id: string; fired: boolean }
  | { type: 'system';        text: string }
  | { type: 'agents';        agents: AgentInfo[] }
  | { type: 'tasks';         tasks: TaskRecord[] }
  | { type: 'bg_complete';   task_id: string; text: string }
  | { type: 'bg_progress';   task_id: string; text: string }
  | { type: 'ping';          id: number }
  | { type: 'pong';          id: number }

// ── Client → server frames ────────────────────────────────────────────────────

export type ClientFrame =
  | { type: 'user_message';    text: string }
  | { type: 'interrupt' }
  | { type: 'ping';            id: number }
  | { type: 'pong';            id: number }
  | { type: 'start_agent';     id: string }
  | { type: 'terminate_agent'; id: string }
  | { type: 'cancel_task';     id: string }

// ── Shared payloads ───────────────────────────────────────────────────────────

/** A child agent surfaced to mobile by lair's `agents` event. Mobile reaches
 *  any agent's chat via `ws://<lair-tunnel>/agents/<id>/stream` — there is no
 *  direct port/pubkey/host for an agent because lair proxies all traffic.
 *  `kind` is `"local"` or `"remote"`; advisory, the proxy URL is the same. */
export interface AgentInfo {
  id:      string  // = name; used in the proxy URL
  name:    string
  status:  string  // 'running' | 'stopped' | 'pending'
  kind:    string  // 'local' | 'remote'
  parent?: string  // name of the spawning agent, if any (omitted for operator-spawned roots)
}

/** One git worktree of an agent, from GET /agents/:name/worktrees.
 *  Mirrors lair's agent-side WorktreeMeta. */
export interface WorktreeMeta {
  id:         string  // route-safe id derived from the branch
  branch:     string
  path:       string
  created_at: number
}

/** Mirrors okto_core::TaskRecord. */
export interface TaskRecord {
  task_id:      string
  command:      string
  status:       'running' | 'done' | 'error' | 'cancelled'
  started_at:   number
  completed_at: number | null
  summary:      string | null
  cost_usd:     number | null
  /** Present when a monitor is attached — the model is woken with this task's
   *  output at most this often (seconds). Absent for plain background tasks. */
  wake_interval_secs?: number | null
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
