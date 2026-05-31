import { Fragment, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { check, type Update } from '@tauri-apps/plugin-updater'
import { relaunch } from '@tauri-apps/plugin-process'
import { parseQrPayload, formatQrPayload, type QrPayload } from './qr'
import {
  encodeClientFrame, parseServerEvent,
  type AgentInfo, type ServerEvent, type TaskRecord, type WorktreeMeta,
} from './wire'
import { Spinner, View, Text, Touchable } from '@okto/ui'

// The pseudo-id we use for lair itself in the sidebar list. Children's ids are
// their names (per AgentInfo.id); 'lair' is reserved so it can never collide.
const LAIR_ID = 'lair'

// A chat is keyed by a string: LAIR_ID for the parent, an agent name for a
// child, or `<agent>::<worktreeId>` for one of a child agent's git worktrees.
// `::` can't appear in an agent name or worktree id (both are slugs), so it's a
// safe delimiter. The same key threads through every per-agent state map.
const WT_SEP = '::'

interface ParsedKey { agent: string; wt: string | null }
function parseAgentKey(key: string): ParsedKey {
  const i = key.indexOf(WT_SEP)
  if (i < 0) return { agent: key, wt: null }
  return { agent: key.slice(0, i), wt: key.slice(i + WT_SEP.length) }
}
function worktreeKey(agent: string, wt: string): string {
  return `${agent}${WT_SEP}${wt}`
}
/** Proxy sub-path prefix for a chat key. `''` for the lair root, `/agents/<n>`
 *  for a child, `/agents/<n>/worktrees/<wt>` for a worktree. Append `/history`,
 *  `/clear`, `/stream`, etc. */
function agentProxyPath(key: string): string {
  const { agent, wt } = parseAgentKey(key)
  if (agent === LAIR_ID) return ''
  let base = `/agents/${encodeURIComponent(agent)}`
  if (wt) base += `/worktrees/${encodeURIComponent(wt)}`
  return base
}

// Sidebar is user-resizable by dragging its right edge, clamped to this range.
const SIDEBAR_MIN_WIDTH = 200
const SIDEBAR_MAX_WIDTH = 420
const SIDEBAR_DEFAULT_WIDTH = 264
const SIDEBAR_WIDTH_KEY = 'okto.sidebarWidth'

// Stable empty defaults — important: `itemsByAgent[id] ?? []` would mint a new
// array reference every render, which causes the chat-scroll useEffect to
// fire on every keystroke. Sharing one frozen array keeps reference equality.
const EMPTY_ITEMS:  Message[] = []

// How long to keep the interrupt button locked after the user clicks it,
// before assuming the server's `interrupt_ack` was lost and re-enabling.
// Matches mobile's stopAckTimerRef behavior in mobile/App.tsx.
const STOP_ACK_TIMEOUT_MS = 3000

// How long to keep a task's STOP button latched in "STOPPING" before assuming
// the cancel_task_ack was lost on a WS hiccup. Matches mobile's
// CANCEL_ACK_TIMEOUT_MS.
const CANCEL_ACK_TIMEOUT_MS = 6000

// Delay between consecutive history-replay appends on first load. Tuned so
// each Row's brief render overlaps with the next bubble starting to render —
// fast enough that long histories finish quickly, slow enough that a viewer
// perceives motion rather than a flicker. Mirrors mobile's
// HISTORY_STAGGER_MS.
const HISTORY_STAGGER_MS = 35

// Retry delay for /history when the native Noise proxy isn't ready yet
// (transient on launch / reconnect). One retry, then we surface an error.
// Mirrors mobile's 600 ms backoff inside loadHistory.
const HISTORY_RETRY_MS = 600

const EMPTY_TASKS: TaskRecord[] = []

// localStorage key for the persisted client state. Bump the suffix on
// incompatible schema changes so old blobs are silently ignored instead of
// crashing the hydrate path. v2: ChatItem (discriminated union) replaced by
// flat Message[] mirroring mobile (server-authoritative history via
// GET /history reconcile).
const STORAGE_KEY = 'okto.desktop.state.v2'

/** What we serialize to localStorage between launches. Notably *not*
 *  persisted: connStatus (rebuilt from WS state on reconnect), the tunnel
 *  port (a fresh ephemeral one is bound by the Tauri side every launch),
 *  and the transient interrupt/cancel latches.
 *
 *  This is a *flash-prevention cache*, not the source of truth — the chat
 *  shell renders the cached messages on launch so there's no blank-state
 *  flicker, then GET /history reconciles via an LCP merge (mirrors mobile's
 *  historyCache.ts + loadHistory flow). */
interface PersistedState {
  qrPayload?:    QrPayload
  itemsByAgent?: Record<string, Message[]>
  draftByAgent?: Record<string, string>
  tasksByAgent?: Record<string, TaskRecord[]>
  activeAgent?:  string
}

/** Debounce window for the save effect — coalesce bursts of state updates
 *  (e.g. each text-delta during streaming) into one write. */
const PERSIST_DEBOUNCE_MS = 500

/** Read the stored session once at module load. Doing this synchronously
 *  (not in a useEffect) lets us seed the App's initial state from the
 *  stored values, so the very first render already shows the restored
 *  chat shell with status='reconnecting' — no flash of the connect form. */
const initialStored: PersistedState | null = (() => {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (!raw) return null
    const parsed = JSON.parse(raw)
    return parsed && typeof parsed === 'object' ? parsed as PersistedState : null
  } catch {
    return null
  }
})()

// ── Chat item model ──────────────────────────────────────────────────────────
//
// Mirrors mobile's `Message` shape so the server-authoritative GET /history
// payload (which carries `cost_usd` on the assistant message and projects
// each tool_use into a single flattened row with text + output) reconciles
// cleanly via an LCP merge. We don't render one row per ServerEvent — that
// would scroll the user past every `text` delta. Instead `text` deltas
// accumulate into the currently-streaming assistant message (matched by id),
// tool lifecycle (use → output[*] → result) folds into a single tool message
// whose id is the wire tool_use_id, and cost is stamped onto the last
// assistant message at done/interrupted (not its own row).

interface Message {
  id:        string
  role:      'user' | 'assistant' | 'tool' | 'interrupted' | 'error' | 'bg_complete' | 'bg_progress'
  text:      string
  /** Per-turn cost, attached to the last assistant message at done/interrupted. */
  cost?:     number
  /** Accumulated tool output (live stdout from tool_output, replaced by tool_result). */
  output?:   string
  /** True while a tool is mid-execution; cleared on tool_result / done / interrupted. */
  running?:  boolean
  /** Denormalized for the renderer so Row never closes over the full list. */
  prevRole?: Message['role']
}

// Monotonic local id for messages we mint client-side (user sends, errors,
// fresh assistant streams that aren't tied to a wire tool_use_id). Tool
// messages reuse the wire tool_use_id directly.
let _id = 0
const uid = (): string => `m${Date.now()}_${++_id}`

/** Stamp prevRole on every message so Row never needs to close over the full
 *  array. Mirrors mobile's withPrevRoles. */
const withPrevRoles = (msgs: Message[]): Message[] =>
  msgs.map((m, i) => ({ ...m, prevRole: i > 0 ? msgs[i - 1].role : undefined }))

/** Append one message to an existing array and re-stamp only the new entry's
 *  prevRole. Used by every fold-event path that doesn't otherwise need to
 *  recompute the whole list. */
const appendMsg = (prev: Message[], msg: Message): Message[] => {
  const stamped = { ...msg, prevRole: prev.length > 0 ? prev[prev.length - 1].role : undefined }
  return [...prev, stamped]
}

type ConnStatus = 'ready' | 'streaming' | 'error' | 'pending'

type Status =
  | { kind: 'idle' }
  // Brand-new connect attempt from a pasted QR — show the connect screen
  // with a "Connecting…" button so the user knows the click registered.
  | { kind: 'connecting';   target: QrPayload }
  // Restoring a known session (auto on launch, or user re-pasting the same
  // QR after Disconnect). Render the chat shell straight away with the
  // restored items so we never flash the connect form.
  | { kind: 'reconnecting'; target: QrPayload }
  | { kind: 'connected';    target: QrPayload; tunnelPort: number; ws: WebSocket }
  | { kind: 'error';        message: string }

function App() {
  // Initial status: if we have a stored QR, start in 'reconnecting' (which
  // renders the chat shell directly) rather than 'idle' (which would flash
  // the connect screen for a tick before the auto-reconnect kicks in).
  const [status, setStatus] = useState<Status>(() =>
    initialStored?.qrPayload
      ? { kind: 'reconnecting', target: initialStored.qrPayload }
      : { kind: 'idle' }
  )
  const [qrInput, setQrInput] = useState<string>(() =>
    initialStored?.qrPayload ? formatQrPayload(initialStored.qrPayload) : ''
  )
  const [agents, setAgents]   = useState<AgentInfo[]>([])
  const [activeAgent, setActiveAgent] = useState<string>(() =>
    initialStored?.activeAgent ?? LAIR_ID
  )

  // Per-agent state, keyed by AgentInfo.id (or LAIR_ID). Keeping these
  // separate lets a child's stream keep accumulating while the user is
  // looking at another tab — switching back restores the in-progress
  // transcript, draft, and connection status untouched.
  const [itemsByAgent,      setItemsByAgent]      = useState<Record<string, Message[]>>(() => {
    // Hydrate the flash-prevention cache and re-stamp prevRole on every row —
    // cheaper than serializing it (denormalized, fully derivable) and keeps
    // the persisted blob shape narrowly the on-screen Message shape minus
    // the cache field.
    const raw = initialStored?.itemsByAgent ?? {}
    const out: Record<string, Message[]> = {}
    for (const [k, v] of Object.entries(raw)) out[k] = withPrevRoles(v)
    return out
  })
  const [draftByAgent,      setDraftByAgent]      = useState<Record<string, string>>(() =>
    initialStored?.draftByAgent ?? {}
  )
  const [connStatusByAgent, setConnStatusByAgent] = useState<Record<string, ConnStatus>>({ [LAIR_ID]: 'pending' })
  // stopSent locks the interrupt button at reduced opacity from click until
  // the server's `interrupt_ack` (or our 3 s fallback timer). Mirrors
  // mobile's stopSent/stopAckTimerRef.
  const [stopSentByAgent,   setStopSentByAgent]   = useState<Record<string, boolean>>({})

  // Background-task registry per agent — lair pushes one `tasks` frame on
  // every spawn/completion/cancellation. Mobile lives in mobile/App.tsx as
  // `masterTasks` + per-child `tasks`.
  const [tasksByAgent,      setTasksByAgent]      = useState<Record<string, TaskRecord[]>>(() =>
    initialStored?.tasksByAgent ?? {}
  )

  // Per-agent model name, learned from the server's `ready` frame. Empty
  // string until the first ready lands (footer renders blank in that window).
  const [modelByAgent,      setModelByAgent]      = useState<Record<string, string>>({})

  // Git worktrees per agent (keyed by agent name), fetched from
  // GET /agents/:name/worktrees. Rendered as indented rows under the agent.
  const [worktreesByAgent,  setWorktreesByAgent]  = useState<Record<string, WorktreeMeta[]>>({})
  // When the user clicks "＋ worktree" on an agent row, this holds that agent's
  // name and an inline branch-name input appears beneath it.
  const [creatingWtFor,     setCreatingWtFor]     = useState<string | null>(null)
  const [newBranchDraft,    setNewBranchDraft]    = useState<string>('')

  // Optimistic latch for the per-task STOP button. One Set shared across
  // agents — task_ids are server-allocated UUIDs so they don't collide.
  const [cancellingIds,     setCancellingIds]     = useState<Set<string>>(() => new Set())
  const cancelTimersRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map())

  // Visibility for the Background Tasks modal.
  const [showTasksModal,    setShowTasksModal]    = useState(false)

  // ── Auto-update ────────────────────────────────────────────────────────────
  //
  // Tauri's updater plugin checks the `desktop-latest` GitHub release's
  // latest.json (configured in tauri.conf.json) against this build's version.
  // We check once on launch (quiet) and expose a manual "Check for updates"
  // button. `updateRef` holds the live Update handle so the button's install
  // click can download + apply it; `updateState` drives the button label.
  const updateRef = useRef<Update | null>(null)
  const [updateState, setUpdateState] = useState<
    | { kind: 'idle' }
    | { kind: 'checking' }
    | { kind: 'available'; version: string }
    | { kind: 'uptodate' }
    | { kind: 'downloading' }
    | { kind: 'error'; message: string }
  >({ kind: 'idle' })

  const checkForUpdates = async (manual: boolean) => {
    // Don't interrupt an in-flight download with a fresh check.
    if (updateState.kind === 'downloading') return
    if (manual) setUpdateState({ kind: 'checking' })
    try {
      const u = await check()
      if (u) {
        updateRef.current = u
        setUpdateState({ kind: 'available', version: u.version })
      } else {
        updateRef.current = null
        // Stay quiet on the silent launch check; only confirm "up to date"
        // when the user explicitly asked.
        if (manual) setUpdateState({ kind: 'uptodate' })
        else setUpdateState({ kind: 'idle' })
      }
    } catch (e) {
      // The updater throws if it can't reach the endpoint or the manifest is
      // missing (e.g. before the first desktop-latest release exists). Surface
      // it only on a manual check so launch stays silent.
      if (manual) setUpdateState({ kind: 'error', message: String(e) })
      else setUpdateState({ kind: 'idle' })
    }
  }

  const installUpdate = async () => {
    const u = updateRef.current
    if (!u) return
    setUpdateState({ kind: 'downloading' })
    try {
      await u.downloadAndInstall()
      // Relaunch into the freshly-installed version. The process exits here.
      await relaunch()
    } catch (e) {
      setUpdateState({ kind: 'error', message: String(e) })
    }
  }

  // Quiet check once on launch. Runs regardless of connection state so the
  // prompt can appear even on the connect screen.
  useEffect(() => {
    void checkForUpdates(false)
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // User-draggable sidebar width (persisted, clamped to MIN..MAX).
  const [sidebarWidth, setSidebarWidth] = useState<number>(() => {
    const saved = Number(localStorage.getItem(SIDEBAR_WIDTH_KEY))
    return saved >= SIDEBAR_MIN_WIDTH && saved <= SIDEBAR_MAX_WIDTH
      ? saved
      : SIDEBAR_DEFAULT_WIDTH
  })

  useEffect(() => {
    localStorage.setItem(SIDEBAR_WIDTH_KEY, String(sidebarWidth))
  }, [sidebarWidth])

  const startSidebarResize = (e: React.MouseEvent) => {
    e.preventDefault()
    const onMove = (ev: MouseEvent) => {
      const w = Math.min(SIDEBAR_MAX_WIDTH, Math.max(SIDEBAR_MIN_WIDTH, ev.clientX))
      setSidebarWidth(w)
    }
    const onUp = () => {
      document.removeEventListener('mousemove', onMove)
      document.removeEventListener('mouseup', onUp)
      document.body.style.cursor = ''
      document.body.style.userSelect = ''
    }
    document.addEventListener('mousemove', onMove)
    document.addEventListener('mouseup', onUp)
    document.body.style.cursor = 'col-resize'
    document.body.style.userSelect = 'none'
  }

  // Derived per-agent slices for the active tab. EMPTY_ITEMS is a stable
  // reference so [items] dep checks don't fire when an unrelated tab updates.
  const items      = itemsByAgent[activeAgent]      ?? EMPTY_ITEMS
  const draft      = draftByAgent[activeAgent]      ?? ''
  const connStatus = connStatusByAgent[activeAgent] ?? 'pending'
  const stopSent   = stopSentByAgent[activeAgent]   ?? false
  const tasks      = tasksByAgent[activeAgent]      ?? EMPTY_TASKS
  const model      = modelByAgent[activeAgent]      ?? ''

  const chatRef = useRef<HTMLDivElement>(null)
  // Stick to the bottom while the user is at the bottom; let them scroll up.
  const stickToBottomRef = useRef(true)

  // WebSocket layout:
  //
  //   masterWsRef           → ws://tunnel/stream            (always-on after
  //                                                          connect; feeds
  //                                                          the agents list
  //                                                          *and* lair chat)
  //   childWsRefs.get(name) → ws://tunnel/agents/<id>/stream (opened on first
  //                                                          select, stays
  //                                                          open until
  //                                                          disconnect)
  //
  // Holding the child sockets open in the background lets an agent's stream
  // keep landing in its per-agent slot while the user is looking at a
  // different tab — switch back and the chat is current, no replay seam.
  // Mirrors mobile's per-child ChatPane behavior.
  const masterWsRef = useRef<WebSocket | null>(null)
  const childWsRefs = useRef<Map<string, WebSocket>>(new Map())
  // Loopback port of the live Noise tunnel. Kept in a ref (not just `status`)
  // because the WS onmessage closures are bound at connect time and would
  // otherwise read a stale `status`; the reconcile-on-`done` path needs the
  // current port.
  const tunnelPortRef = useRef<number | null>(null)
  // Per-agent fallback timers that re-enable the interrupt button if the
  // server's interrupt_ack never arrives. Keyed by agent id.
  const stopAckTimersRef = useRef<Record<string, ReturnType<typeof setTimeout> | null>>({})

  // Per-agent streaming state. `streamingId` is the Message id that the next
  // text delta should land on (or extend, if `hasAssistant` is true); bumped
  // at every turn boundary (user send / tool_use / done / interrupted / error)
  // so a fresh assistant bubble materializes for each new chunk of model text.
  // Mirrors mobile's streamingIdRef + hasAssistantMsgRef, keyed by agent.
  const streamingIdRef = useRef<Record<string, string>>({})
  const hasAssistantRef = useRef<Record<string, boolean>>({})

  // Per-agent shadow message-list used during a `ready{resumed:true}` →
  // `replay_end` window. While set, buffered events fold into this shadow
  // instead of the visible list; replay_end swaps it in atomically so the
  // user doesn't see a truncate-then-rebuild flash on mid-turn reconnect.
  // Mirrors mobile's replayingRef — wrap the array in an object so a captured
  // empty list (`[]`) doesn't read as falsy in `if (replayingRef.current[id])`.
  const replayingRef = useRef<Record<string, { msgs: Message[] } | null>>({})

  // Per-agent in-flight /history AbortController so a second connect (or a
  // tab switch that re-fires the loader) cancels the prior fetch instead of
  // racing with it. Mirrors mobile's historyAbortRef.
  const historyAbortRef = useRef<Record<string, AbortController | null>>({})

  // Per-agent stagger queue for the divergent /history suffix — first new
  // row drops in synchronously, the rest land one per HISTORY_STAGGER_MS so
  // each bubble's render cascades instead of a wall. Mirrors mobile's
  // historyStaggerRef.
  const historyStaggerRef = useRef<Record<string, {
    queue: Message[]
    timer: ReturnType<typeof setTimeout> | null
  }>>({})

  // Per-agent "history has loaded" tracker. Mirrors mobile's historyReadyFor
  // but used informationally — the WS-open gate is the await on
  // loadHistoryForAgent in connectInternal / openChildWs, not this flag.
  // Consumed by the renderer to suppress the "Awaiting your first message"
  // empty-state until we *know* the conversation is empty, rather than
  // flashing it during the /history fetch. State (not a ref) so an empty
  // history → no setItemsByAgent → no re-render still flips the empty-state.
  const [historyReady, setHistoryReady] = useState<Record<string, boolean>>({})

  // Last QrPayload that produced an OPEN master WS — used both as the
  // "same lair?" check inside connect() and as the qrPayload we persist
  // so the next launch can auto-reconnect. Seeded from the stored session
  // so the save effect doesn't lose track of it before reconnect lands.
  const lastQrPayloadRef = useRef<QrPayload | null>(initialStored?.qrPayload ?? null)
  // Guards the hydrate effect (so it runs exactly once) and the save effect
  // (which we skip until hydration has completed, otherwise the first
  // empty render would clobber our stored state with defaults).
  const hydratedRef = useRef(false)

  const setDraft = (s: string) => {
    setDraftByAgent(prev => ({ ...prev, [activeAgent]: s }))
  }

  useEffect(() => {
    const el = chatRef.current
    if (!el) return
    const onScroll = () => {
      const dist = el.scrollHeight - el.scrollTop - el.clientHeight
      stickToBottomRef.current = dist < 80
    }
    el.addEventListener('scroll', onScroll)
    return () => el.removeEventListener('scroll', onScroll)
  }, [status.kind])

  useEffect(() => {
    if (stickToBottomRef.current && chatRef.current) {
      chatRef.current.scrollTop = chatRef.current.scrollHeight
    }
  }, [items])

  // On agent switch, snap the new tab's chat to its bottom so the user
  // lands on the latest content instead of mid-scroll from the previous
  // tab's scroll position.
  useEffect(() => {
    stickToBottomRef.current = true
    if (chatRef.current) chatRef.current.scrollTop = chatRef.current.scrollHeight
  }, [activeAgent])

  // ── Persistence ──────────────────────────────────────────────────────────
  //
  // State is already populated from `initialStored` via lazy useState
  // initializers — this effect only needs to kick off the auto-reconnect
  // so the renderer has a live WS to send through. The render is already
  // showing the chat shell (status='reconnecting'), so no flash of the
  // connect form happens while the WS is opening.
  useEffect(() => {
    if (hydratedRef.current) return
    hydratedRef.current = true
    if (!initialStored?.qrPayload) return
    const childToReopen = initialStored.activeAgent && initialStored.activeAgent !== LAIR_ID
      ? initialStored.activeAgent
      : null
    // Fire and forget — failures surface via setStatus({kind:'error'})
    // and the user lands on the connect screen with the QR already filled.
    connectInternal(initialStored.qrPayload, /* preserveState */ true, childToReopen)
      .catch(() => { /* already surfaced via status */ })
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // Save on every meaningful state change, debounced so streaming text
  // deltas don't write to disk hundreds of times per second. Skipped
  // pre-hydration so the initial render's empty defaults can't clobber a
  // freshly-loaded session.
  useEffect(() => {
    if (!hydratedRef.current) return
    const t = setTimeout(() => {
      const payload: PersistedState = {
        qrPayload:    lastQrPayloadRef.current ?? undefined,
        itemsByAgent,
        draftByAgent,
        tasksByAgent,
        activeAgent,
      }
      try { localStorage.setItem(STORAGE_KEY, JSON.stringify(payload)) }
      catch { /* over quota — drop this save, next one will retry */ }
    }, PERSIST_DEBOUNCE_MS)
    return () => clearTimeout(t)
  }, [itemsByAgent, draftByAgent, tasksByAgent, activeAgent])

  const clearStopLock = (agentId: string) => {
    const t = stopAckTimersRef.current[agentId]
    if (t) { clearTimeout(t); stopAckTimersRef.current[agentId] = null }
    setStopSentByAgent(prev => prev[agentId] ? { ...prev, [agentId]: false } : prev)
  }

  const clearCancelTimer = (taskId: string) => {
    const t = cancelTimersRef.current.get(taskId)
    if (t != null) { clearTimeout(t); cancelTimersRef.current.delete(taskId) }
  }

  const releaseCancel = (taskId: string) => {
    clearCancelTimer(taskId)
    setCancellingIds(prev => {
      if (!prev.has(taskId)) return prev
      const next = new Set(prev)
      next.delete(taskId)
      return next
    })
  }

  // Reconcile the optimistic STOP-button latch against the authoritative task
  // registry: drop any latched id whose task is gone or no longer running.
  // Mirrors mobile's useCancelGuard.reconcile.
  const reconcileCancelling = (taskList: TaskRecord[]) => {
    setCancellingIds(prev => {
      if (prev.size === 0) return prev
      let next: Set<string> | null = null
      for (const id of prev) {
        const t = taskList.find(x => x.task_id === id)
        if (t == null || t.status !== 'running') {
          if (next == null) next = new Set(prev)
          next.delete(id)
          clearCancelTimer(id)
        }
      }
      return next ?? prev
    })
  }

  const requestCancelTask = (taskId: string) => {
    const ws = activeWs()
    if (!ws || ws.readyState !== WebSocket.OPEN) return
    ws.send(encodeClientFrame({ type: 'cancel_task', id: taskId }))
    setCancellingIds(prev => prev.has(taskId) ? prev : new Set(prev).add(taskId))
    clearCancelTimer(taskId)
    cancelTimersRef.current.set(taskId, setTimeout(() => releaseCancel(taskId), CANCEL_ACK_TIMEOUT_MS))
  }

  /** Per-agent `setItemsByAgent` shim that respects the shadow-replay buffer:
   *  while a `ready{resumed:true}` → `replay_end` window is open for this
   *  agent, buffered events fold into the shadow Message[] instead of the
   *  visible list; `replay_end` then swaps the shadow in atomically (see
   *  the `replay_end` case below). Mirrors mobile's applyMsgs. */
  const applyMsgs = (agentId: string, updater: (prev: Message[]) => Message[]) => {
    const shadow = replayingRef.current[agentId]
    if (shadow) {
      shadow.msgs = updater(shadow.msgs)
      return
    }
    setItemsByAgent(prev => ({ ...prev, [agentId]: updater(prev[agentId] ?? []) }))
  }

  /** Drains the staggered history-replay queue for one agent — one append per
   *  tick so each new Row renders one after the next rather than all at once.
   *  Idempotent against live-stream events that may race in (dedupe by id).
   *  Mirrors mobile's tickHistoryStagger. */
  const tickHistoryStagger = (agentId: string) => {
    const stagger = historyStaggerRef.current[agentId]
    if (!stagger) return
    const next = stagger.queue.shift()
    if (next === undefined) {
      stagger.timer = null
      return
    }
    applyMsgs(agentId, prev => prev.some(m => m.id === next.id)
      ? prev
      : withPrevRoles([...prev, next]))
    stagger.timer = stagger.queue.length > 0
      ? setTimeout(() => tickHistoryStagger(agentId), HISTORY_STAGGER_MS)
      : null
  }

  /** Fetch /history for an agent (lair root or child via the proxy) and
   *  reconcile against the agent's current visible message list using a
   *  longest-common-prefix merge. Matched rows are kept verbatim so they
   *  retain their ids and React's reconciler reuses the mounted bubbles
   *  (a naive full replace would re-key every row and re-render). For the
   *  divergent suffix we drop the first in immediately and queue the rest
   *  for the staggered ticker so each bubble appears one after the next.
   *
   *  Sets `historyReadyByAgent[agentId]` on success so the WS gate can open.
   *  Mirrors mobile's loadHistory + LCP merge. */
  const loadHistoryForAgent = async (
    agentId:    string,
    tunnelPort: number,
    attempt:    number = 0,
  ): Promise<void> => {
    historyAbortRef.current[agentId]?.abort()
    const controller = new AbortController()
    historyAbortRef.current[agentId] = controller

    const base = `http://127.0.0.1:${tunnelPort}${agentProxyPath(agentId)}`

    try {
      const res = await fetch(`${base}/history`, { signal: controller.signal })
      const data = await res.json() as {
        messages: Array<{ role: string; text: string; cost_usd?: number; output?: string }>
      }
      // Superseded by a later loadHistoryForAgent call (e.g. quick tab
      // switch) — drop this response on the floor.
      if (historyAbortRef.current[agentId] !== controller) return

      const msgs: Message[] = data.messages.map((m, i) => ({
        id:   `h${agentId}_${i}`,
        role: m.role as Message['role'],
        text: m.text,
        ...(m.cost_usd != null ? { cost: m.cost_usd } : {}),
        ...(m.output    != null ? { output: m.output } : {}),
      }))

      // Reset any prior stagger queue for this agent — we're about to
      // recompute the divergent suffix from scratch.
      const stagger = historyStaggerRef.current[agentId] ?? { queue: [], timer: null }
      if (stagger.timer) { clearTimeout(stagger.timer); stagger.timer = null }
      stagger.queue = []
      historyStaggerRef.current[agentId] = stagger

      // Tool rows are matched leniently: live tool text is "label (arg)" and
      // server projects it as "name(arg)", so a strict text compare would
      // diverge on the first tool row and force a full replace (losing the
      // mounted chip's expanded/collapsed state and remounting every later
      // row). They're the same event — match by role and keep the live row.
      const eq = (a: Message, b: Message): boolean => {
        if (a.role !== b.role) return false
        if (a.role === 'tool') return true
        return a.text === b.text && a.cost === b.cost && a.output === b.output
      }

      applyMsgs(agentId, (cur) => {
        let common = 0
        while (common < cur.length && common < msgs.length && eq(cur[common], msgs[common])) common++
        if (common === msgs.length) {
          // Identical to what we already have — nothing to apply.
          if (common === cur.length) return cur
          // Server history is a strict prefix of ours. Either a turn is
          // streaming/replaying and its rows aren't persisted to /history yet
          // (keep them — we're live-ahead), or the conversation was
          // cleared/truncated on the server, e.g. from another client (adopt
          // the server's shorter history). /history is authoritative when
          // idle, so truncate the stale tail.
          if (replayingRef.current[agentId] || connStatusByAgent[agentId] === 'streaming') return cur
          return cur.slice(0, common)
        }
        const suffix = msgs.slice(common)
        // Single new row — append directly; no need to engage the ticker.
        if (suffix.length === 1) {
          return withPrevRoles([...cur.slice(0, common), suffix[0]])
        }
        // Multiple new rows — first goes in synchronously so the user sees
        // motion immediately; the rest land one per stagger tick.
        stagger.queue = suffix.slice(1)
        stagger.timer = setTimeout(() => tickHistoryStagger(agentId), HISTORY_STAGGER_MS)
        return withPrevRoles([...cur.slice(0, common), suffix[0]])
      })

      setHistoryReady(prev => prev[agentId] ? prev : { ...prev, [agentId]: true })
    } catch (e) {
      if ((e as Error).name === 'AbortError') return
      // The native Noise proxy may not be ready to accept connections
      // immediately after the tunnel reconnects (transient on launch /
      // foreground return), which would surface here as a network error.
      // One retry then re-throw so the caller (connectInternal /
      // openChildWs) can skip its WS open and surface the error. We *await*
      // the retry rather than scheduling-and-resolving so the caller knows
      // history is settled when this promise resolves.
      if (attempt === 0) {
        await new Promise(r => setTimeout(r, HISTORY_RETRY_MS))
        if (historyAbortRef.current[agentId] !== controller) return
        await loadHistoryForAgent(agentId, tunnelPort, 1)
      } else {
        setConnStatusByAgent(prev => ({ ...prev, [agentId]: 'error' }))
        throw e
      }
    }
  }

  // Apply a chat-stream event to a specific agent's slot. Runs regardless of
  // which tab is currently visible — that's what makes per-agent persistence
  // work; events flow into their own slot and the active tab just renders
  // whichever one is selected.
  //
  // The fold is inlined here (rather than a pure `foldEvent`) because tracking
  // the active streaming-assistant id requires touching per-agent refs —
  // mirrors mobile's handleStreamEvent.
  const applyChatEvent = (agentId: string, ev: ServerEvent) => {
    // `tasks` and `cancel_task_ack` don't belong in the chat scroll — they
    // drive the background-tasks registry / STOP-button latch instead.
    if (ev.type === 'tasks') {
      setTasksByAgent(prev => ({ ...prev, [agentId]: ev.tasks }))
      reconcileCancelling(ev.tasks)
      return
    }
    if (ev.type === 'cancel_task_ack') {
      clearCancelTimer(ev.id)
      // Server had nothing live to cancel — release the latch immediately.
      // If fired=true, leave it latched; the next `tasks` frame moving the
      // task off `running` will release via reconcileCancelling.
      if (!ev.fired) releaseCancel(ev.id)
      return
    }

    // Ensure per-agent streaming-id state exists before any case touches it.
    if (streamingIdRef.current[agentId] === undefined) streamingIdRef.current[agentId] = uid()
    if (hasAssistantRef.current[agentId] === undefined) hasAssistantRef.current[agentId] = false

    switch (ev.type) {
      case 'ready': {
        if (ev.model) {
          setModelByAgent(prev => prev[agentId] === ev.model ? prev : { ...prev, [agentId]: ev.model })
        }
        // Mid-turn reconnect: the server is about to replay every buffered
        // event for the in-flight turn. Anchor a shadow copy of the current
        // visible list — buffered events fold into it via applyMsgs above —
        // and atomically swap when `replay_end` lands. Without this the user
        // sees the visible list truncate to pre-turn state, then rebuild as
        // each frame arrives.
        //
        // Read the anchor through the React updater rather than the closed-
        // over `itemsByAgent`: the WS handler was wired in connectInternal
        // which closes over that render's stale list, but the post-/history
        // updated list lives in the next render's state — only `prev` inside
        // a setter sees it.
        if (ev.resumed) {
          setItemsByAgent(prev => {
            replayingRef.current[agentId] = { msgs: prev[agentId] ?? [] }
            return prev
          })
        } else {
          // Not resumed — drop any stale shadow from a previous mid-replay
          // drop (defensive: the WS was closed before replay_end arrived).
          replayingRef.current[agentId] = null
        }
        setConnStatusByAgent(prev => ({ ...prev, [agentId]: 'ready' }))
        break
      }
      case 'replay_end': {
        // Server's mid-turn replay has fully reseeded our shadow. Swap it
        // into view as a single update so the user sees one transition (the
        // streaming turn picks up from there as fresh events arrive).
        const shadow = replayingRef.current[agentId]
        if (shadow) {
          replayingRef.current[agentId] = null
          setItemsByAgent(prev => ({ ...prev, [agentId]: withPrevRoles(shadow.msgs) }))
        }
        break
      }
      case 'text': {
        const chunk = ev.text
        const sid = streamingIdRef.current[agentId]
        if (!hasAssistantRef.current[agentId]) {
          hasAssistantRef.current[agentId] = true
          applyMsgs(agentId, prev => appendMsg(prev, { id: sid, role: 'assistant', text: chunk }))
        } else {
          applyMsgs(agentId, prev => prev.map(m => m.id === sid ? { ...m, text: m.text + chunk } : m))
        }
        setConnStatusByAgent(prev => ({ ...prev, [agentId]: 'streaming' }))
        break
      }
      case 'tool_use': {
        // Bump streaming id so the *next* text block becomes a fresh
        // assistant message after the tool, not appended to pre-tool text.
        hasAssistantRef.current[agentId] = false
        streamingIdRef.current[agentId] = uid()
        const firstVal = ev.input && typeof ev.input === 'object'
          ? String(Object.values(ev.input as Record<string, unknown>)[0] ?? '').trim()
          : ''
        const label = ev.display ?? humanizeTool(ev.tool)
        const toolText = firstVal ? `${label} (${firstVal})` : label
        // Use the wire tool_use_id as the Message id so subsequent
        // tool_output/tool_result events route to the right bubble even
        // when the model emits multiple tool_use blocks in one turn. The
        // model can emit several tool_use blocks at once and they all
        // stream to the client before the server executes any of them —
        // mark this tool `running` only if no earlier tool is still
        // executing, otherwise it's queued.
        applyMsgs(agentId, prev => {
          const anyRunning = prev.some(m => m.running)
          return appendMsg(prev, { id: ev.tool_use_id, role: 'tool', text: toolText, running: !anyRunning })
        })
        setConnStatusByAgent(prev => ({ ...prev, [agentId]: 'streaming' }))
        break
      }
      case 'tool_output': {
        const toolId = ev.tool_use_id
        applyMsgs(agentId, prev => prev.map(m =>
          m.id === toolId ? { ...m, output: (m.output ?? '') + ev.line + '\n' } : m
        ))
        break
      }
      case 'tool_result': {
        const toolId = ev.tool_use_id
        const out = stringifyResult(ev.output)
        applyMsgs(agentId, prev => {
          const completedIdx = prev.findIndex(m => m.id === toolId)
          if (completedIdx < 0) return prev
          // Promote the next queued tool (earliest after the completed one,
          // not running, no output yet) to active execution. Tools run in
          // emission order so the next queued slot is always after the
          // current one in the array.
          let nextQueuedIdx = -1
          for (let i = completedIdx + 1; i < prev.length; i++) {
            const m = prev[i]
            if (m.role === 'tool' && !m.running && m.output === undefined) {
              nextQueuedIdx = i
              break
            }
          }
          return prev.map((m, i) => {
            if (i === completedIdx)  return { ...m, output: out, running: false }
            if (i === nextQueuedIdx) return { ...m, running: true }
            return m
          })
        })
        break
      }
      case 'done': {
        const cost = ev.cost_usd
        applyMsgs(agentId, prev => {
          // Defensive: clear any leftover running flags (e.g. dropped
          // tool_result frames) so the chip's pulse doesn't stay on after
          // the turn ends.
          const base = prev.some(m => m.running)
            ? prev.map(m => m.running ? { ...m, running: false } : m)
            : prev
          for (let i = base.length - 1; i >= 0; i--) {
            if (base[i].role === 'assistant') {
              const next = base.slice()
              next[i] = { ...next[i], cost }
              return next
            }
          }
          return base
        })
        // Turn boundary — anything streamed after this (auto-turn from a
        // bg_complete, or the next user turn) must start a fresh assistant
        // bubble, not append to the one we just sealed.
        hasAssistantRef.current[agentId] = false
        streamingIdRef.current[agentId] = uid()
        setConnStatusByAgent(prev => ({ ...prev, [agentId]: 'ready' }))
        clearStopLock(agentId)
        // Reconcile against the authoritative server history at the turn
        // boundary. If another client cleared/edited the conversation while
        // we were idle, our local copy is stale until the next reconcile —
        // this keeps both clients converged on lair's history without
        // waiting for an incidental trigger (tab switch / reconnect).
        if (tunnelPortRef.current != null) void loadHistoryForAgent(agentId, tunnelPortRef.current)
        break
      }
      case 'interrupted': {
        const cost = ev.cost_usd
        applyMsgs(agentId, prev => {
          // Any tool still marked as running won't get a tool_result now.
          let base = prev.map(m => m.running ? { ...m, running: false } : m)
          for (let i = base.length - 1; i >= 0; i--) {
            if (base[i].role === 'assistant') {
              base = base.slice()
              base[i] = { ...base[i], cost }
              break
            }
          }
          return appendMsg(base, { id: uid(), role: 'interrupted', text: 'interrupted' })
        })
        hasAssistantRef.current[agentId] = false
        streamingIdRef.current[agentId] = uid()
        setConnStatusByAgent(prev => ({ ...prev, [agentId]: 'ready' }))
        clearStopLock(agentId)
        break
      }
      case 'interrupt_ack':
        clearStopLock(agentId)
        break
      case 'error': {
        applyMsgs(agentId, prev => appendMsg(
          prev.map(m => m.running ? { ...m, running: false } : m),
          { id: uid(), role: 'error', text: ev.message },
        ))
        hasAssistantRef.current[agentId] = false
        streamingIdRef.current[agentId] = uid()
        setConnStatusByAgent(prev => ({ ...prev, [agentId]: 'error' }))
        break
      }
      case 'bg_complete': {
        // Dedupe by task id so a subsequent /history reload (which also
        // contains this row) is a no-op rather than a duplicate. The id
        // matches what mobile uses so the LCP merge tracks the same anchor.
        const id = `bg_${ev.task_id}`
        applyMsgs(agentId, prev => prev.some(m => m.id === id)
          ? prev
          : appendMsg(prev, { id, role: 'bg_complete', text: ev.text }))
        break
      }
      case 'bg_progress': {
        // Distinct per event (monitored tasks emit one per stdout chunk);
        // text matches the persisted bg_progress row so /history reconciles
        // cleanly.
        applyMsgs(agentId, prev => appendMsg(prev, { id: uid(), role: 'bg_progress', text: ev.text }))
        break
      }
      // system / agents / ping / pong / tasks / cancel_task_ack: handled
      // upstream or not surfaced in the chat log.
      default:
        break
    }
  }

  // The master WS is special: it always handles `agents` (which only lair
  // emits) regardless of which tab is visible. Its chat events feed lair's
  // slot. Children never push `agents`, so their handler is plain applyChatEvent.
  const handleMasterEvent = (ev: ServerEvent) => {
    if (ev.type === 'agents') {
      setAgents(ev.agents)
      // Refresh each agent's worktree list so the sidebar nesting stays current
      // (new agents, or worktrees added from another client).
      for (const a of ev.agents) void fetchWorktrees(a.id)
      return
    }
    applyChatEvent(LAIR_ID, ev)
  }

  /** Reusable connect path used by both user-driven `connect()` and the
   *  auto-reconnect on launch from persisted state. When `preserveState`
   *  is true we leave items/drafts/tasks alone (caller asserts this is the
   *  same lair as before, so the restored history is still valid); when
   *  false we wipe to a clean slate. `childToReopen` lets the caller name
   *  a non-lair active agent whose child WS should be opened immediately
   *  on master-open so the user's saved tab is usable. */
  const connectInternal = async (
    target:       QrPayload,
    preserveState:boolean,
    childToReopen:string | null,
  ) => {
    // `reconnecting` = known lair → render the chat shell with the
    // restored items. `connecting` = brand-new attempt → keep the connect
    // screen visible with a "Connecting…" button. Same WS path under both.
    setStatus({ kind: preserveState ? 'reconnecting' : 'connecting', target })
    // History-ready gate always resets on connect — the server's canonical
    // /history must reseed each agent's list before its WS opens (else a
    // mid-turn replay could clobber the streaming bubble). Per-agent
    // /history aborts also reset; any in-flight fetch from a prior tunnel
    // is canceled below as part of the abort sweep.
    setHistoryReady({})
    for (const c of Object.values(historyAbortRef.current)) {
      try { c?.abort() } catch {}
    }
    historyAbortRef.current = {}
    for (const s of Object.values(historyStaggerRef.current)) {
      if (s?.timer) clearTimeout(s.timer)
    }
    historyStaggerRef.current = {}
    replayingRef.current = {}

    if (!preserveState) {
      setItemsByAgent({})
      setDraftByAgent({})
      setConnStatusByAgent({ [LAIR_ID]: 'pending' })
      setStopSentByAgent({})
      setTasksByAgent({})
      setCancellingIds(new Set())
      for (const t of cancelTimersRef.current.values()) clearTimeout(t)
      cancelTimersRef.current.clear()
      stopAckTimersRef.current = {}
      setAgents([])
      setActiveAgent(LAIR_ID)
      setShowTasksModal(false)
    } else {
      // Restored session — reset only the transient slices that depend on
      // a live WS. Persistent slots stay intact.
      setConnStatusByAgent(prev => ({ ...prev, [LAIR_ID]: 'pending' }))
      setStopSentByAgent({})
      setCancellingIds(new Set())
      for (const t of cancelTimersRef.current.values()) clearTimeout(t)
      cancelTimersRef.current.clear()
      stopAckTimersRef.current = {}
      setShowTasksModal(false)
    }
    try {
      const tunnelPort = await invoke<number>('noise_connect', {
        host:            target.host,
        port:            target.port,
        serverPubkeyB32: target.pk,
      })
      tunnelPortRef.current = tunnelPort
      // Gate: fetch /history *before* opening the master WS. The server
      // replays buffered events on mid-turn reconnect, and a replay that
      // landed first would be clobbered by /history's later reconcile.
      // Mirrors mobile's `historyReadyFor === baseUrl` WS gate.
      await loadHistoryForAgent(LAIR_ID, tunnelPort)

      const ws = new WebSocket(`ws://127.0.0.1:${tunnelPort}/stream`)
      masterWsRef.current = ws
      ws.onopen  = () => {
        setStatus({ kind: 'connected', target, tunnelPort, ws })
        // Mark this QR as the canonical "what we're connected to" so the
        // save effect persists it and future connect() attempts can detect
        // same-vs-different lair.
        lastQrPayloadRef.current = target
        // Re-open the WS for the restored active child (if any) so the
        // user's saved tab is immediately interactive — otherwise typing
        // into a child chat would silently no-op until they re-click it.
        if (childToReopen) {
          // Fire-and-forget — openChildWs is async only because it gates on
          // /history; nothing here needs to await its result.
          openChildWs(tunnelPort, childToReopen).catch(() => {})
        }
      }
      ws.onclose = () => {
        masterWsRef.current = null
        tunnelPortRef.current = null
        // Master is gone — close any child WSes; they all sit on the same
        // (now-defunct) Noise proxy. Their onclose handlers will flip each
        // slot's connStatus to 'pending' for the next reconnect.
        for (const w of childWsRefs.current.values()) {
          try { w.close() } catch {}
        }
        childWsRefs.current.clear()
        setStatus({ kind: 'idle' })
        setConnStatusByAgent(prev => ({ ...prev, [LAIR_ID]: 'pending' }))
        setAgents([])
      }
      ws.onerror = () => {
        setStatus({ kind: 'error', message: 'WebSocket error' })
        setConnStatusByAgent(prev => ({ ...prev, [LAIR_ID]: 'error' }))
      }
      ws.onmessage = (e) => {
        const data = typeof e.data === 'string' ? e.data : ''
        const ev = parseServerEvent(data)
        if (!ev) return
        if (ev.type === 'ping') {
          ws.send(encodeClientFrame({ type: 'pong', id: ev.id }))
          return
        }
        handleMasterEvent(ev)
      }
    } catch (e) {
      setStatus({ kind: 'error', message: String(e) })
      setConnStatusByAgent(prev => ({ ...prev, [LAIR_ID]: 'error' }))
    }
  }

  const connect = async () => {
    const target = parseQrPayload(qrInput)
    if (!target) {
      setStatus({ kind: 'error', message: 'Invalid QR payload — expected 2:<host>:<port>:<pubkey>' })
      return
    }
    // If the user re-pasted the same QR (same host/port/pubkey), keep the
    // restored session intact and just reopen the tunnel. A new QR points
    // at a different lair — wipe and start over.
    const last = lastQrPayloadRef.current
    const sameLair = !!last
      && last.host === target.host
      && last.port === target.port
      && last.pk   === target.pk
    const child = sameLair && activeAgent !== LAIR_ID ? activeAgent : null
    await connectInternal(target, sameLair, child)
  }

  // `key` is a child agent name or a `<agent>::<worktree>` composite. The proxy
  // path (and thus the stream URL) is derived from it via agentProxyPath.
  const openChildWs = async (tunnelPort: number, key: string): Promise<WebSocket | null> => {
    // If we already have an open or in-flight WS for this child, reuse it
    // — opening a second would just race with the first.
    const existing = childWsRefs.current.get(key)
    if (existing && existing.readyState <= WebSocket.OPEN) return existing

    // Gate: fetch /history for the child *before* opening its WS so the
    // server's mid-turn replay can't land ahead of the canonical history.
    // No-op if a previous open already loaded history for this agent
    // (loadHistoryForAgent re-fetches on each call so the user always sees
    // fresh server state on tab re-entry, but the existing-WS short-circuit
    // above means we only get here when there's no live WS).
    await loadHistoryForAgent(key, tunnelPort)

    const ws = new WebSocket(`ws://127.0.0.1:${tunnelPort}${agentProxyPath(key)}/stream`)
    childWsRefs.current.set(key, ws)
    ws.onclose = () => {
      if (childWsRefs.current.get(key) === ws) childWsRefs.current.delete(key)
      setConnStatusByAgent(prev => ({ ...prev, [key]: 'pending' }))
    }
    ws.onerror = () => {
      setConnStatusByAgent(prev => ({ ...prev, [key]: 'error' }))
    }
    ws.onmessage = (e) => {
      const data = typeof e.data === 'string' ? e.data : ''
      const ev = parseServerEvent(data)
      if (!ev) return
      if (ev.type === 'ping') {
        ws.send(encodeClientFrame({ type: 'pong', id: ev.id }))
        return
      }
      // Always write to *this* agent's slot — even if the user has navigated
      // away. That's what makes the in-progress chat survive a tab switch.
      applyChatEvent(key, ev)
    }
    return ws
  }

  const selectAgent = (id: string) => {
    if (status.kind !== 'connected') return
    if (id === activeAgent) return
    setActiveAgent(id)
    if (id === LAIR_ID) {
      // Master is already open; sync the status pill to its actual state
      // (preserve a streaming/ready status if a turn is mid-flight).
      setConnStatusByAgent(prev => ({
        ...prev,
        [LAIR_ID]: masterWsRef.current?.readyState === WebSocket.OPEN ? (prev[LAIR_ID] ?? 'ready') : 'pending',
      }))
    } else {
      // First time opening this child? Spin up a WS; otherwise reuse the
      // one we already have streaming into its slot.
      if (!childWsRefs.current.has(id)) {
        setConnStatusByAgent(prev => ({ ...prev, [id]: 'pending' }))
      }
      // Fire-and-forget — openChildWs is async only because it awaits
      // /history before opening the WS; selectAgent just navigates.
      openChildWs(status.tunnelPort, id).catch(() => {})
    }
  }

  const activeWs = (): WebSocket | null => {
    if (activeAgent === LAIR_ID) return masterWsRef.current
    return childWsRefs.current.get(activeAgent) ?? null
  }

  // ── Worktrees ──────────────────────────────────────────────────────────────

  const fetchWorktrees = async (agentName: string) => {
    const port = tunnelPortRef.current
    if (port == null) return
    try {
      const res = await fetch(`http://127.0.0.1:${port}/agents/${encodeURIComponent(agentName)}/worktrees`)
      if (!res.ok) return
      const data = await res.json() as WorktreeMeta[]
      setWorktreesByAgent(prev => ({ ...prev, [agentName]: data }))
    } catch { /* transient — next agents push retries */ }
  }

  const submitNewWorktree = async (agentName: string) => {
    const branch = newBranchDraft.trim()
    if (!branch) return
    const port = tunnelPortRef.current
    if (port == null) return
    setCreatingWtFor(null)
    setNewBranchDraft('')
    try {
      const res = await fetch(
        `http://127.0.0.1:${port}/agents/${encodeURIComponent(agentName)}/worktrees`,
        { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ branch }) },
      )
      if (!res.ok) return
      const meta = await res.json() as WorktreeMeta
      await fetchWorktrees(agentName)
      selectAgent(worktreeKey(agentName, meta.id))
    } catch { /* fire-and-forget; list won't show the new tree if it failed */ }
  }

  const deleteWorktree = async (agentName: string, wtId: string) => {
    const port = tunnelPortRef.current
    if (port == null) return
    const key = worktreeKey(agentName, wtId)
    // Close its WS and forget local state up front so the UI reacts instantly.
    try { childWsRefs.current.get(key)?.close() } catch { /* already gone */ }
    childWsRefs.current.delete(key)
    if (activeAgent === key) setActiveAgent(agentName)
    setItemsByAgent(prev => { const n = { ...prev }; delete n[key]; return n })
    try {
      await fetch(
        `http://127.0.0.1:${port}/agents/${encodeURIComponent(agentName)}/worktrees/${encodeURIComponent(wtId)}`,
        { method: 'DELETE' },
      )
    } catch { /* fire-and-forget */ }
    await fetchWorktrees(agentName)
  }

  const send = () => {
    if (status.kind !== 'connected') return
    const text = draft.trim()
    if (!text) return
    const ws = activeWs()
    if (!ws || ws.readyState !== WebSocket.OPEN) return
    ws.send(encodeClientFrame({ type: 'user_message', text }))
    const agentId = activeAgent
    // Turn boundary: bump streaming id + clear hasAssistant so the next text
    // delta starts a fresh assistant bubble (mirrors mobile's pre-send reset).
    streamingIdRef.current[agentId] = uid()
    hasAssistantRef.current[agentId] = false
    setItemsByAgent(prev => ({
      ...prev,
      [agentId]: appendMsg(prev[agentId] ?? [], { id: uid(), role: 'user', text }),
    }))
    setDraftByAgent(prev => ({ ...prev, [agentId]: '' }))
    // Optimistically flip to 'streaming' so the orbit indicator + stop button
    // appear the moment Send is pressed, not only when the first text delta
    // lands (which can be a noticeable beat with model thinking time). The
    // first server event will reaffirm 'streaming'; done/interrupted/error
    // flip it back to 'ready' as usual.
    setConnStatusByAgent(prev => ({ ...prev, [agentId]: 'streaming' }))
    stickToBottomRef.current = true
  }

  const interrupt = () => {
    const ws = activeWs()
    if (!ws || ws.readyState !== WebSocket.OPEN) return
    if (stopSent) return  // double-tap guard — wait for ack or fallback
    ws.send(encodeClientFrame({ type: 'interrupt' }))
    const agentId = activeAgent
    setStopSentByAgent(prev => ({ ...prev, [agentId]: true }))
    // Clear any stale timer for this agent, then arm a 3 s fallback.
    const prevTimer = stopAckTimersRef.current[agentId]
    if (prevTimer) clearTimeout(prevTimer)
    stopAckTimersRef.current[agentId] = setTimeout(() => {
      stopAckTimersRef.current[agentId] = null
      setStopSentByAgent(prev => ({ ...prev, [agentId]: false }))
    }, STOP_ACK_TIMEOUT_MS)
  }

  const disconnect = () => {
    // Tear down every child WS we hold; their onclose handlers will flip
    // each slot's connStatus to 'pending'.
    for (const ws of childWsRefs.current.values()) {
      try { ws.close() } catch {}
    }
    childWsRefs.current.clear()
    // Cancel any outstanding stop-ack and cancel-task timers; their state
    // will be reset on the next connect.
    for (const k of Object.keys(stopAckTimersRef.current)) {
      const t = stopAckTimersRef.current[k]
      if (t) clearTimeout(t)
    }
    stopAckTimersRef.current = {}
    for (const t of cancelTimersRef.current.values()) clearTimeout(t)
    cancelTimersRef.current.clear()
    // Abort any in-flight /history fetch and drain pending stagger queues
    // so a quick disconnect-then-reconnect doesn't double-apply rows.
    for (const c of Object.values(historyAbortRef.current)) {
      try { c?.abort() } catch {}
    }
    historyAbortRef.current = {}
    for (const s of Object.values(historyStaggerRef.current)) {
      if (s?.timer) clearTimeout(s.timer)
    }
    historyStaggerRef.current = {}
    replayingRef.current = {}
    setHistoryReady({})
    setShowTasksModal(false)
    // Close the master WS via the ref, not status.ws — during 'reconnecting'
    // (or 'connecting') the WS is mid-handshake and not yet attached to the
    // status. Closing through the ref handles all three live states.
    if (masterWsRef.current) {
      try { masterWsRef.current.close() } catch {}
      // ws.onclose will flip status → 'idle'.
    } else {
      setStatus({ kind: 'idle' })
    }
  }

  const clearChat = () => {
    if (status.kind !== 'connected') return
    const agentId = activeAgent
    // Wipe the visible log immediately so the click feels instant.
    setItemsByAgent(prev => ({ ...prev, [agentId]: [] }))
    // Ask the server to drop its conversation state too — without this the
    // next message would resume on top of the previous transcript. lair's
    // /clear lives at the root; child clears go through the proxy.
    const url = `http://127.0.0.1:${status.tunnelPort}${agentProxyPath(agentId)}/clear`
    fetch(url, { method: 'POST' }).catch(() => { /* fire-and-forget */ })
  }

  // Render the chat shell for both `connected` (live WS) and `reconnecting`
  // (restored from storage, WS still opening). The latter shows the stored
  // chat history immediately while the status pill stays orange until lair
  // sends its `ready`.
  if (status.kind !== 'connected' && status.kind !== 'reconnecting') {
    return (
      <ConnectScreen
        qrInput={qrInput}
        setQrInput={setQrInput}
        onConnect={connect}
        status={status}
      />
    )
  }

  const activeLabel = (() => {
    if (activeAgent === LAIR_ID) return 'Lair'
    const { agent, wt } = parseAgentKey(activeAgent)
    const agentName = agents.find(a => a.id === agent)?.name ?? agent
    if (!wt) return agentName
    const branch = (worktreesByAgent[agent] ?? []).find(w => w.id === wt)?.branch ?? wt
    return `${agentName} / ${branch}`
  })()

  return (
    <View
      position="relative" height="100vh" width="100vw" overflow="hidden" backgroundColor="$background50" color="$typography900"
      style={{ display: 'grid', gridTemplateColumns: `${sidebarWidth}px 1fr` }}
    >
      <Sidebar
        agents={agents}
        activeAgent={activeAgent}
        onSelect={selectAgent}
        worktreesByAgent={worktreesByAgent}
        creatingWtFor={creatingWtFor}
        newBranchDraft={newBranchDraft}
        onStartCreateWt={(name) => { setCreatingWtFor(name); setNewBranchDraft('') }}
        onCancelCreateWt={() => { setCreatingWtFor(null); setNewBranchDraft('') }}
        onChangeBranchDraft={setNewBranchDraft}
        onSubmitWt={submitNewWorktree}
        onDeleteWt={deleteWorktree}
      />
      <View
        position="absolute" top={0} bottom={0} zIndex={10} width={12} cursor="col-resize" hoverStyle={{ backgroundColor: 'rgba(219,234,254,0.4)' }}
        style={{ left: sidebarWidth - 6 }}
        onMouseDown={startSidebarResize}
        title="Drag to resize sidebar"
      />
      <View flexDirection="column" minWidth={0} backgroundColor="$background0">
        <View flexDirection="row" alignItems="center" gap={10} borderBottomWidth={1} borderColor="$outline100" paddingHorizontal={20} paddingVertical={12}>
          <Text minWidth={0} flex={1} fontSize={16} fontWeight="600" color="$typography900">{activeLabel}</Text>
          <Touchable
            borderRadius={6} paddingHorizontal={10} paddingVertical={4} fontSize={12} fontWeight="500" color="$error600" hoverStyle={{ backgroundColor: '$error50' }}
            onPress={disconnect}
          >
            Disconnect
          </Touchable>
          <StatusPill status={connStatus} />
          <Touchable
            borderRadius={6} borderWidth={1} borderColor="$outline200" paddingHorizontal={10} paddingVertical={4} fontSize={12} fontWeight="500" color="$typography600" hoverStyle={{ backgroundColor: '$background100' }}
            onPress={clearChat}
            disabled={connStatus !== 'ready' || items.length === 0}
            title="Clear chat history"
          >
            Clear
          </Touchable>
          <TasksButton tasks={tasks} onPress={() => setShowTasksModal(v => !v)} />
          <UpdateButton
            state={updateState}
            onCheck={() => checkForUpdates(true)}
            onInstall={installUpdate}
          />
        </View>

        <View flex={1} overflow="auto" paddingHorizontal={20} paddingVertical={16} ref={chatRef}>
          {items.length === 0 && historyReady[activeAgent] && (
            // Only show the "empty conversation" prompt once /history has
            // confirmed the agent really has no messages — otherwise the
            // text flashes for the duration of the GET /history while the
            // chat is just waiting on the server's authoritative reply.
            <View marginTop={48} textAlign="center" fontSize={14} color="$typography400">Awaiting your first message</View>
          )}
          {items.map(item => <Row key={item.id} item={item} />)}
        </View>

        <InputBar
          draft={draft}
          setDraft={setDraft}
          onSend={send}
          onInterrupt={interrupt}
          streaming={connStatus === 'streaming'}
          stopSent={stopSent}
          model={model}
          completionsBase={
            status.kind === 'connected'
              ? activeAgent === LAIR_ID
                ? `http://127.0.0.1:${status.tunnelPort}`
                : `http://127.0.0.1:${status.tunnelPort}/agents/${encodeURIComponent(activeAgent)}`
              : null
          }
        />
      </View>

      <TasksDrawer
        visible={showTasksModal}
        agentLabel={activeLabel}
        tasks={tasks}
        cancellingIds={cancellingIds}
        onClose={() => setShowTasksModal(false)}
        onCancel={requestCancelTask}
      />
    </View>
  )
}

// ── Fold helpers ─────────────────────────────────────────────────────────────
//
// The per-event fold itself is inlined into `applyChatEvent` above (because
// each case touches per-agent streamingId/hasAssistant refs); only the
// shared shape-coercion helpers live here.

function stringifyResult(out: unknown): string {
  if (typeof out === 'string') return out
  return JSON.stringify(out)
}

/** Fallback when the server didn't supply a display label — mirrors
 *  `okto_core::derive_display_label`. */
function humanizeTool(name: string): string {
  const bare = name.includes('__') ? name.slice(name.lastIndexOf('__') + 2) : name
  const [first, ...rest] = bare.split('_').filter(Boolean)
  if (!first) return name
  const verb = first.endsWith('e') && first.length > 1 ? `${first.slice(0, -1)}ing` : `${first}ing`
  const phrase = [verb[0].toUpperCase() + verb.slice(1), ...rest].join(' ')
  return phrase
}

// ── Components ──────────────────────────────────────────────────────────────

function ConnectScreen({
  qrInput, setQrInput, onConnect, status,
}: {
  qrInput: string
  setQrInput: (s: string) => void
  onConnect: () => void
  status: Status
}) {
  const connecting = status.kind === 'connecting'
  const onKey = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
      e.preventDefault()
      onConnect()
    }
  }
  return (
    <View height="100vh" width="100vw" alignItems="center" justifyContent="center" backgroundColor="$background50">
      <View width="100%" maxWidth={448} borderRadius={16} borderWidth={1} borderColor="$outline100" backgroundColor="$background0" padding={36}>
        <Text fontSize={24} fontWeight="700" color="$typography900">okto</Text>
        <View marginTop={12} height={1} width={40} backgroundColor="$primary500" />
        <Text marginTop={12} fontSize={12} fontWeight="500" textTransform="uppercase" color="$typography400">Desktop</Text>
        <Text marginTop={16} fontSize={14} color="$typography500">
          Paste the session QR payload printed by lair on startup.
        </Text>
        <textarea
          className="mt-5 h-24 w-full resize-none rounded-lg border border-outline-200 bg-background-50 px-3.5 py-3 font-mono text-sm text-typography-800 outline-none transition focus:border-primary-500 focus:bg-background-0"
          value={qrInput}
          onChange={(e) => setQrInput(e.currentTarget.value)}
          onKeyDown={onKey}
          placeholder="2:1.2.3.4:9000:ABCDEF…"
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
        />
        <Touchable
          marginTop={16} width="100%" borderRadius={8} backgroundColor="$primary500" paddingVertical={10} fontSize={14} fontWeight="600" color="$typography0" hoverStyle={{ backgroundColor: '$primary600' }}
          onPress={onConnect}
          disabled={connecting || !qrInput.trim()}
        >
          {connecting ? 'Connecting…' : 'Connect'}
        </Touchable>
        {status.kind === 'error' && (
          <Text marginTop={12} fontSize={14} color="$error600">{status.message}</Text>
        )}
      </View>
    </View>
  )
}

function Sidebar({
  agents, activeAgent, onSelect,
  worktreesByAgent, creatingWtFor, newBranchDraft,
  onStartCreateWt, onCancelCreateWt, onChangeBranchDraft, onSubmitWt, onDeleteWt,
}: {
  agents: AgentInfo[]
  activeAgent: string
  onSelect: (id: string) => void
  worktreesByAgent: Record<string, WorktreeMeta[]>
  creatingWtFor: string | null
  newBranchDraft: string
  onStartCreateWt: (agentName: string) => void
  onCancelCreateWt: () => void
  onChangeBranchDraft: (v: string) => void
  onSubmitWt: (agentName: string) => void
  onDeleteWt: (agentName: string, wtId: string) => void
}) {
  return (
    <View flexDirection="column" borderRightWidth={1} borderColor="$outline100" backgroundColor="$background50" paddingHorizontal={10} paddingVertical={12}>
      <View flexDirection="column" gap={2}>
        <AgentRow
          id={LAIR_ID}
          name="Lair"
          statusText="main"
          statusKind="ready"
          active={activeAgent === LAIR_ID}
          onSelect={onSelect}
        />
      </View>

      <Text marginBottom={4} marginTop={20} paddingHorizontal={8} fontSize={11} fontWeight="600" textTransform="uppercase" color="$typography400">Agents</Text>
      <View flexDirection="column" gap={2}>
        {agents.length === 0 && (
          <View paddingHorizontal={8} paddingVertical={6} fontSize={12} color="$typography400">No child agents</View>
        )}
        {agents.map(a => {
          const worktrees = worktreesByAgent[a.id] ?? []
          return (
            <Fragment key={a.id}>
              <AgentRow
                id={a.id}
                name={a.name}
                statusText={a.status}
                statusKind={agentStatusKind(a.status)}
                active={activeAgent === a.id}
                onSelect={onSelect}
                onAddWorktree={() => onStartCreateWt(a.id)}
              />
              {worktrees.map(wt => (
                <AgentRow
                  key={`${a.id}${WT_SEP}${wt.id}`}
                  id={worktreeKey(a.id, wt.id)}
                  name={wt.branch}
                  statusText="worktree"
                  statusKind="ready"
                  active={activeAgent === worktreeKey(a.id, wt.id)}
                  onSelect={onSelect}
                  worktree
                  onDelete={() => onDeleteWt(a.id, wt.id)}
                />
              ))}
              {creatingWtFor === a.id && (
                <View paddingHorizontal={8} paddingVertical={4}>
                  <input
                    className="w-full rounded-md border border-outline-200 bg-background-0 px-2 py-1 text-xs text-typography-800 outline-none focus:border-primary-500"
                    autoFocus
                    placeholder="new branch name…"
                    value={newBranchDraft}
                    onChange={(e) => onChangeBranchDraft(e.currentTarget.value)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') onSubmitWt(a.id)
                      if (e.key === 'Escape') onCancelCreateWt()
                    }}
                    onBlur={onCancelCreateWt}
                  />
                </View>
              )}
            </Fragment>
          )
        })}
      </View>
    </View>
  )
}

// Status dot color by kind — shared by the sidebar rows.
const DOT_CLASS: Record<'ready' | 'pending' | 'error', string> = {
  ready:   'bg-success-500',
  pending: 'bg-warning-500',
  error:   'bg-error-500',
}

function AgentRow({
  id, name, statusText, statusKind, active, onSelect,
  worktree, onAddWorktree, onDelete,
}: {
  id: string
  name: string
  statusText: string
  statusKind: 'ready' | 'pending' | 'error'
  active: boolean
  onSelect: (id: string) => void
  worktree?: boolean
  onAddWorktree?: () => void
  onDelete?: () => void
}) {
  return (
    <View>
      <Touchable
        className={`group flex w-full items-center gap-2 rounded-md py-1.5 pr-2 text-left text-sm transition ${
          active ? 'bg-primary-50 font-medium text-primary-800' : 'text-typography-700 hover:bg-background-100'
        } ${worktree ? 'pl-5' : 'pl-2'}`}
        onPress={() => onSelect(id)}
      >
        <Text className={`h-1.5 w-1.5 shrink-0 rounded-full ${DOT_CLASS[statusKind]}`} />
        <Text minWidth={0} flex={1}>
          {worktree && <Text color="$typography400">⌥&nbsp;</Text>}
          {name}
        </Text>
        <Text flexShrink={0} alignItems="center">
          {onAddWorktree && (
            <Text
              borderRadius={6} paddingHorizontal={4} color="$typography400" opacity={0} hoverStyle={{ color: '$primary600' }}
              title="Add worktree"
              onPress={(e) => { e.stopPropagation(); onAddWorktree() }}
            >＋</Text>
          )}
          {onDelete && (
            <Text
              className="rounded px-1 text-typography-400 opacity-0 transition hover:text-error-600 group-hover:opacity-100"
              title="Delete worktree (and its branch)"
              onPress={(e) => { e.stopPropagation(); onDelete() }}
            >✕</Text>
          )}
          {!onAddWorktree && !onDelete && (
            <Text fontSize={11} color="$typography400">{statusText}</Text>
          )}
        </Text>
      </Touchable>
    </View>
  )
}

function agentStatusKind(status: string): 'ready' | 'pending' | 'error' {
  if (status === 'running') return 'ready'
  if (status === 'pending') return 'pending'
  return 'error'
}

// ── Background tasks ────────────────────────────────────────────────────────

function TasksButton({ tasks, onClick }: { tasks: TaskRecord[]; onClick: () => void }) {
  const running = tasks.filter(t => t.status === 'running').length
  return (
    <Touchable
      className={`flex items-center gap-1.5 rounded-md border px-2.5 py-1 text-xs font-medium transition ${
        running > 0
          ? 'border-primary-200 bg-primary-50 text-primary-700'
          : 'border-outline-200 text-typography-600 hover:bg-background-100'
      }`}
      onPress={onClick}
      title="Background tasks"
    >
      <Text className={`h-1.5 w-1.5 rounded-full ${running > 0 ? 'bg-primary-500' : 'bg-typography-300'}`} />
      {running > 0 ? `Tasks · ${running}` : 'Tasks'}
    </Touchable>
  )
}

type UpdateState =
  | { kind: 'idle' }
  | { kind: 'checking' }
  | { kind: 'available'; version: string }
  | { kind: 'uptodate' }
  | { kind: 'downloading' }
  | { kind: 'error'; message: string }

/** Toolbar control for app updates. When an update is available it becomes a
 *  prominent "Update to vX.Y.Z" install button; otherwise it's a quiet
 *  "Check for updates" button that reports its result inline. */
function UpdateButton({
  state, onCheck, onInstall,
}: {
  state:     UpdateState
  onCheck:   () => void
  onInstall: () => void
}) {
  if (state.kind === 'available') {
    return (
      <Touchable
        borderRadius={6} backgroundColor="$primary500" paddingHorizontal={10} paddingVertical={4} fontSize={12} fontWeight="600" color="$typography0" hoverStyle={{ backgroundColor: '$primary600' }}
        onPress={onInstall}
        title={`Download and install v${state.version}, then restart`}
      >
        ↓ Update to v{state.version}
      </Touchable>
    )
  }
  if (state.kind === 'downloading') {
    return (
      <Touchable borderRadius={6} borderWidth={1} borderColor="$outline200" paddingHorizontal={10} paddingVertical={4} fontSize={12} fontWeight="500" color="$typography400" disabled title="Downloading update…">
        Updating…
      </Touchable>
    )
  }
  const label =
    state.kind === 'checking' ? 'Checking…'
    : state.kind === 'uptodate' ? 'Up to date'
    : state.kind === 'error' ? 'Update failed'
    : 'Check for updates'
  return (
    <Touchable
      borderRadius={6} borderWidth={1} borderColor="$outline200" paddingHorizontal={10} paddingVertical={4} fontSize={12} fontWeight="500" color="$typography600" hoverStyle={{ backgroundColor: '$background100' }}
      onPress={onCheck}
      disabled={state.kind === 'checking'}
      title={state.kind === 'error' ? state.message : 'Check for a newer version'}
    >
      {label}
    </Touchable>
  )
}

function TasksDrawer({
  visible, agentLabel, tasks, cancellingIds, onClose, onCancel,
}: {
  visible:       boolean
  agentLabel:    string
  tasks:         TaskRecord[]
  cancellingIds: Set<string>
  onClose:       () => void
  onCancel:      (taskId: string) => void
}) {
  // Close on Escape — desktop convention.
  useEffect(() => {
    if (!visible) return
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [visible, onClose])

  // Always render so the drawer can transition cleanly in/out; visibility is
  // driven by an `open` class that toggles transform + backdrop opacity.
  const sorted = tasks.slice().sort((a, b) => {
    if (a.status === 'running' && b.status !== 'running') return -1
    if (b.status === 'running' && a.status !== 'running') return 1
    return b.started_at - a.started_at
  })

  return (
    <>
      <View
        className={`fixed inset-0 z-40 bg-typography-950/20 transition-opacity duration-200 ${visible ? 'opacity-100' : 'pointer-events-none opacity-0'}`}
        onPress={onClose}
      />
      <View
        className={`fixed right-0 top-0 z-50 flex h-screen w-[380px] flex-col border-l border-outline-100 bg-background-0 shadow-soft-3 transition-transform duration-200 ${visible ? 'translate-x-0' : 'translate-x-full'}`}
        aria-hidden={!visible}
      >
        <View alignItems="flex-start" justifyContent="space-between" borderBottomWidth={1} borderColor="$outline100" paddingHorizontal={20} paddingVertical={16}>
          <View>
            <View fontSize={14} fontWeight="600" color="$typography900">Background Tasks</View>
            <View fontSize={12} color="$typography400">{agentLabel}</View>
          </View>
          <Touchable borderRadius={4} padding={4} color="$typography400" hoverStyle={{ backgroundColor: '$background100', color: '$typography700' }} onPress={onClose} title="Close (Esc)">✕</Touchable>
        </View>
        <View flex={1} overflow="scroll" paddingHorizontal={16} paddingVertical={12}>
          {sorted.length === 0 ? (
            <View marginTop={32} textAlign="center" fontSize={14} color="$typography400">No background tasks</View>
          ) : (
            sorted.map(t => (
              <TaskRow
                key={t.task_id}
                task={t}
                cancelling={cancellingIds.has(t.task_id)}
                onCancel={() => onCancel(t.task_id)}
              />
            ))
          )}
        </View>
      </View>
    </>
  )
}

function TaskRow({
  task, cancelling, onCancel,
}: {
  task: TaskRecord
  cancelling: boolean
  onCancel: () => void
}) {
  const [expanded, setExpanded] = useState(false)
  const isRunning = task.status === 'running'
  const ts = task.completed_at != null
    ? relativeTime(task.completed_at)
    : relativeTime(task.started_at)
  const statusKind = taskStatusKind(task.status)
  return (
    <View borderRadius={8} borderWidth={1} borderColor="$outline100" backgroundColor="$background50" padding={12}>
      <View alignItems="center" gap={8}>
        <Text className={`inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-[10px] font-semibold uppercase tracking-wide ${TASK_TAG_CLASS[statusKind]}`}>
          <Text className={`h-1.5 w-1.5 rounded-full ${TASK_DOT_CLASS[statusKind]}`} />
          <Text>{task.status.toUpperCase()}</Text>
        </Text>
        {task.wake_interval_secs != null && (
          <Text fontSize={10} fontWeight="500" textTransform="uppercase" color="$primary600">◈ Monitored</Text>
        )}
        <Text fontSize={11} color="$typography400">{ts}</Text>
        {isRunning && (
          <Touchable
            marginLeft="auto" borderRadius={6} backgroundColor="$error50" paddingHorizontal={8} paddingVertical={2} fontSize={11} fontWeight="600" color="$error600" hoverStyle={{ backgroundColor: '$error100' }}
            onPress={onCancel}
            disabled={cancelling}
          >
            {cancelling ? 'Stopping' : 'Stop'}
          </Touchable>
        )}
      </View>
      <Touchable
        marginTop={8} width="100%" textAlign="left"
        onPress={() => setExpanded(v => !v)}
        title={expanded ? 'Collapse' : 'Expand'}
      >
        <View className={`font-mono text-xs text-typography-800 ${expanded ? '' : 'line-clamp-2'}`}>{task.command}</View>
        {task.summary && task.summary.length > 0 && (
          <View className={`mt-1 text-xs text-typography-500 ${expanded ? '' : 'line-clamp-2'}`}>{task.summary}</View>
        )}
        {task.cost_usd != null && task.cost_usd > 0 && (
          <View marginTop={4} fontSize={11} color="$typography400">{formatCost(task.cost_usd)}</View>
        )}
      </Touchable>
    </View>
  )
}

// Background-task status tag/dot palette.
const TASK_TAG_CLASS: Record<'running' | 'done' | 'cancelled' | 'error', string> = {
  running:   'bg-primary-50 text-primary-700',
  done:      'bg-success-50 text-success-700',
  cancelled: 'bg-background-100 text-typography-500',
  error:     'bg-error-50 text-error-700',
}
const TASK_DOT_CLASS: Record<'running' | 'done' | 'cancelled' | 'error', string> = {
  running:   'bg-primary-500',
  done:      'bg-success-500',
  cancelled: 'bg-typography-300',
  error:     'bg-error-500',
}

function taskStatusKind(status: TaskRecord['status']): 'running' | 'done' | 'cancelled' | 'error' {
  if (status === 'running')   return 'running'
  if (status === 'done')      return 'done'
  if (status === 'cancelled') return 'cancelled'
  return 'error'
}

function relativeTime(epochSecs: number): string {
  const delta = Math.max(0, Math.floor(Date.now() / 1000) - epochSecs)
  if (delta < 60)    return `${delta}s ago`
  if (delta < 3600)  return `${Math.floor(delta / 60)}m ago`
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`
  return `${Math.floor(delta / 86400)}d ago`
}

function formatCost(usd: number): string {
  return usd < 0.01 ? `$${usd.toFixed(4)}` : `$${usd.toFixed(2)}`
}

const STATUS_PILL_CLASS: Record<ConnStatus, string> = {
  ready:     'bg-success-50 text-success-700',
  streaming: 'bg-primary-50 text-primary-700',
  error:     'bg-error-50 text-error-700',
  pending:   'bg-warning-50 text-warning-700',
}
const STATUS_DOT_CLASS: Record<ConnStatus, string> = {
  ready:     'bg-success-500',
  streaming: 'bg-primary-500',
  error:     'bg-error-500',
  pending:   'bg-warning-500',
}

function StatusPill({ status }: { status: ConnStatus }) {
  const label = useMemo(() => {
    if (status === 'ready')      return 'Ready'
    if (status === 'streaming')  return 'Streaming'
    if (status === 'error')      return 'Error'
    return 'Connecting'
  }, [status])
  return (
    <Text className={`inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-[11px] font-medium ${STATUS_PILL_CLASS[status]}`}>
      <Text className={`h-1.5 w-1.5 rounded-full ${STATUS_DOT_CLASS[status]}`} />
      <Text>{label}</Text>
    </Text>
  )
}

// ── Markdown rendering ────────────────────────────────────────────────────────
// Minimal renderer mirroring mobile/App.tsx::renderText for the bits the model
// actually emits in chat: fenced code blocks (```ts ... ```), inline code
// (`foo`), **bold**, *italic*, ~~strike~~. Not a general-purpose markdown
// implementation — headings/lists/blockquotes are out of scope here.

const INLINE_MD = /\*\*(.+?)\*\*|__(.+?)__|(?<!\*)\*(?!\*)(.+?)(?<!\*)\*(?!\*)|~~(.+?)~~|`([^`]+)`/gs

function renderInlineSpans(text: string, baseKey: number): ReactNode[] {
  const out: ReactNode[] = []
  let last = 0, m: RegExpExecArray | null, i = 0
  INLINE_MD.lastIndex = 0
  while ((m = INLINE_MD.exec(text)) !== null) {
    if (m.index > last) out.push(<Fragment key={`${baseKey}-${i++}`}>{text.slice(last, m.index)}</Fragment>)
    const k = `${baseKey}-${i++}`
    if      (m[1] != null) out.push(<strong key={k}>{m[1]}</strong>)
    else if (m[2] != null) out.push(<strong key={k}>{m[2]}</strong>)
    else if (m[3] != null) out.push(<em key={k}>{m[3]}</em>)
    else if (m[4] != null) out.push(<s key={k}>{m[4]}</s>)
    else if (m[5] != null) out.push(<Text key={k} borderRadius={6} backgroundColor="$background100" paddingHorizontal={4} paddingVertical={2} fontFamily="$mono" color="$typography800">{m[5]}</Text>)
    last = m.index + m[0].length
  }
  if (last < text.length) out.push(<Fragment key={`${baseKey}-${i++}`}>{text.slice(last)}</Fragment>)
  return out
}

function MarkdownText({ text }: { text: string }) {
  if (!text) return null
  // Split on fenced code blocks first; preserve them as opaque tokens so the
  // inline regex never sees their backticks.
  const segments = text.split(/(```[\s\S]*?```)/g)
  const out: ReactNode[] = []
  let k = 0
  for (const seg of segments) {
    if (!seg) continue
    if (seg.startsWith('```') && seg.endsWith('```') && seg.length >= 6) {
      // Strip optional ```lang\n prefix.
      let lang = ''
      const body = seg.slice(3, -3).replace(/^([a-zA-Z0-9_+-]+)\n/, (_, l) => { lang = l; return '' })
      out.push(
        <View key={`md-${k++}`} marginVertical={8} borderRadius={8} borderWidth={1} borderColor="$outline100" backgroundColor="$background50" padding={12} fontFamily="$mono" fontSize={12} color="$typography800">
          {lang && <Text marginBottom={4} fontSize={10} textTransform="uppercase" color="$typography400">{lang}</Text>}
          <Text>{body}</Text>
        </View>
      )
    } else {
      out.push(<Fragment key={`md-${k++}`}>{renderInlineSpans(seg, k)}</Fragment>)
    }
  }
  return <>{out}</>
}

// ── Row ──────────────────────────────────────────────────────────────────────

function ToolRow({ item }: { item: Message }) {
  // Output hidden by default; user clicks the chip to expand. Local state per
  // chip — the row key is the tool_use_id (stable across re-renders), so
  // collapsed/expanded persists for the chip's lifetime.
  const [expanded, setExpanded] = useState(false)
  const hasOutput = item.output != null && item.output.length > 0
  return (
    <View marginBottom={8}>
      <View className={`overflow-hidden rounded-lg border border-outline-100 border-l-2 ${
        item.running ? 'border-l-primary-500 bg-primary-50/40' : 'border-l-outline-300 bg-background-50'
      }`}>
        <Touchable
          type="button"
          width="100%" alignItems="center" gap={8} paddingHorizontal={12} paddingVertical={8} textAlign="left"
          onPress={() => { if (hasOutput) setExpanded(e => !e) }}
          disabled={!hasOutput}
          aria-expanded={hasOutput ? expanded : undefined}
        >
          {item.running && <Text height={6} width={6} flexShrink={0} borderRadius={999} backgroundColor="$primary500" />}
          {!item.running && item.output === undefined && <Text height={6} width={6} flexShrink={0} borderRadius={999} backgroundColor="$typography300" />}
          <Text minWidth={0} flex={1} fontFamily="$mono" fontSize={12} color="$typography700">{item.text}</Text>
          {hasOutput && (
            <Text className={`shrink-0 text-typography-400 transition-transform ${expanded ? 'rotate-90' : ''}`} aria-hidden="true">▸</Text>
          )}
        </Touchable>
        {expanded && hasOutput && (
          <View borderTopWidth={1} borderColor="$outline100" paddingHorizontal={12} paddingVertical={8} fontFamily="$mono" fontSize={11} color="$typography600">{truncate(item.output!, 4000)}</View>
        )}
      </View>
    </View>
  )
}

function Row({ item }: { item: Message }) {
  switch (item.role) {
    case 'user':
      return (
        <View marginBottom={12} justifyContent="flex-end">
          <View maxWidth="78%" borderRadius={16} borderBottomRightRadius={6} backgroundColor="$primary500" paddingHorizontal={14} paddingVertical={8} fontSize={14} color="$typography0">{item.text}</View>
        </View>
      )
    case 'assistant':
      // Cost (when present) is the turn total stamped onto the last
      // assistant message at done/interrupted — render it as a small label
      // below the text. Mirrors mobile's MessageBubble assistant branch.
      return (
        <View marginBottom={12}>
          <View maxWidth="80%" fontSize={14} color="$typography800">
            <MarkdownText text={item.text} />
            {item.cost != null && (
              <View marginTop={4} fontSize={11} color="$typography400">${item.cost.toFixed(4)}</View>
            )}
          </View>
        </View>
      )
    case 'tool':
      return <ToolRow item={item} />
    case 'interrupted':
      // Cost (if any) lives on the preceding assistant message — this row is
      // just the standalone "● Interrupted" marker.
      return <View marginBottom={8} fontSize={12} fontWeight="500" color="$warning600">● Interrupted</View>
    case 'error':
      return <View marginBottom={8} fontSize={12} fontWeight="500" color="$error600">● {item.text}</View>
    case 'bg_complete':
    case 'bg_progress': {
      // Take just the first line — the persisted body is prefixed with a
      // "Background command <id> completed…" / "[monitor] … produced new
      // output:" header which is enough context; the long body would crowd
      // the chip. Marker differs so the user can spot progress vs. final.
      const firstLine = item.text.split('\n', 1)[0] || item.text
      const marker = item.role === 'bg_progress' ? '◈' : '◇'
      return <View marginBottom={8} fontSize={12} color="$typography500">{marker} {firstLine}</View>
    }
  }
}

function truncate(s: string, n: number): string {
  if (s.length <= n) return s
  return `${s.slice(0, n)}\n…[${s.length - n} more chars]`
}

// Mirrors mobile's parseAtQuery (mobile/App.tsx) so an `@`-prefixed token at the
// tail of the draft drives the file-completion popup. Returning null suppresses
// the popup (no `@`, or the user typed a space after it).
function parseAtQuery(text: string): { atIndex: number; dirPart: string; filePart: string } | null {
  const atIndex = text.lastIndexOf('@')
  if (atIndex === -1) return null
  const query = text.slice(atIndex + 1)
  if (query.includes(' ') || query.includes('\n')) return null
  const lastSlash = query.lastIndexOf('/')
  return lastSlash === -1
    ? { atIndex, dirPart: '', filePart: query }
    : { atIndex, dirPart: query.slice(0, lastSlash + 1), filePart: query.slice(lastSlash + 1) }
}

function InputBar({
  draft, setDraft, onSend, onInterrupt, streaming, stopSent, model, completionsBase,
}: {
  draft: string
  setDraft: (s: string) => void
  onSend: () => void
  onInterrupt: () => void
  streaming: boolean
  stopSent: boolean
  model: string
  completionsBase: string | null
}) {
  const taRef = useRef<HTMLTextAreaElement>(null)
  const [completions,   setCompletions]   = useState<string[]>([])
  const [selectedIdx,   setSelectedIdx]   = useState(0)

  // Auto-grow textarea.
  useEffect(() => {
    const ta = taRef.current
    if (!ta) return
    ta.style.height = 'auto'
    ta.style.height = `${Math.min(ta.scrollHeight, 200)}px`
  }, [draft])

  // Debounced fetch of @-completions — mirrors mobile's effect in
  // mobile/App.tsx. The endpoint is /completions on lair and
  // /agents/<name>/completions on a child (proxied by lair).
  useEffect(() => {
    if (!completionsBase) { setCompletions([]); return }
    const parsed = parseAtQuery(draft)
    if (!parsed) { setCompletions([]); return }
    let cancelled = false
    const timer = setTimeout(() => {
      fetch(`${completionsBase}/completions?dir_part=${encodeURIComponent(parsed.dirPart)}&file_part=${encodeURIComponent(parsed.filePart)}`)
        .then(r => r.json())
        .then((data: string[]) => { if (!cancelled) { setCompletions(data); setSelectedIdx(0) } })
        .catch(() => { if (!cancelled) setCompletions([]) })
    }, 200)
    return () => { cancelled = true; clearTimeout(timer) }
  }, [draft, completionsBase])

  const applyCompletion = (completion: string) => {
    const parsed = parseAtQuery(draft)
    if (!parsed) return
    const newText = draft.slice(0, parsed.atIndex + 1) + completion
    if (completion.endsWith('/')) {
      // Directory — keep the popup open, let the user keep drilling in.
      setDraft(newText)
    } else {
      setDraft(newText + ' ')
      setCompletions([])
    }
    // Refocus so typing continues uninterrupted.
    requestAnimationFrame(() => taRef.current?.focus())
  }

  const onKey = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (completions.length > 0) {
      if (e.key === 'ArrowDown') {
        e.preventDefault()
        setSelectedIdx(i => (i + 1) % completions.length)
        return
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault()
        setSelectedIdx(i => (i - 1 + completions.length) % completions.length)
        return
      }
      if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
        e.preventDefault()
        applyCompletion(completions[selectedIdx] ?? completions[0])
        return
      }
      if (e.key === 'Escape') {
        e.preventDefault()
        setCompletions([])
        return
      }
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      onSend()
    }
  }

  return (
    <View borderTopWidth={1} borderColor="$outline100" paddingHorizontal={16} paddingBottom={12} paddingTop={12}>
      {completions.length > 0 && (
        <View marginBottom={8} maxHeight={192} overflow="scroll" borderRadius={8} borderWidth={1} borderColor="$outline200" backgroundColor="$background0">
          {completions.map((c, i) => (
            <View
              key={c}
              className={`cursor-pointer px-3 py-1.5 font-mono text-xs ${i === selectedIdx ? 'bg-primary-50 text-primary-800' : 'text-typography-700 hover:bg-background-100'}`}
              onMouseDown={(e) => { e.preventDefault(); applyCompletion(c) }}
              onMouseEnter={() => setSelectedIdx(i)}
            >
              {c}
            </View>
          ))}
        </View>
      )}
      <View alignItems="flex-end" gap={8}>
        <textarea
          ref={taRef}
          className="max-h-[200px] min-h-[40px] flex-1 resize-none rounded-xl border border-outline-200 bg-background-50 px-3.5 py-2.5 text-sm text-typography-800 outline-none transition placeholder:text-typography-400 focus:border-primary-500 focus:bg-background-0"
          value={draft}
          onChange={(e) => setDraft(e.currentTarget.value)}
          onKeyDown={onKey}
          rows={1}
          placeholder="Message…"
        />
        {streaming ? (
          // The interrupt button, encircled by a Spinner that conveys the
          // model is generating. Clicking sends an interrupt and locks the
          // button at reduced opacity until the server's interrupt_ack (or 3 s
          // fallback).
          <View position="relative" height={36} width={36} flexShrink={0} alignItems="center" justifyContent="center">
            {!stopSent && (
              <Spinner
                color="#0d9488"
                size={44}
                className="pointer-events-none absolute -inset-1"
              />
            )}
            <Touchable
              className={`flex h-9 w-9 items-center justify-center rounded-full bg-error-500 transition hover:bg-error-600 disabled:cursor-not-allowed ${stopSent ? 'opacity-50' : ''}`}
              onPress={onInterrupt}
              disabled={stopSent}
              title={stopSent ? 'Interrupt sent…' : 'Interrupt'}
            >
              <Text height={10} width={10} borderRadius={2} backgroundColor="$typography0" />
            </Touchable>
          </View>
        ) : (
          <Touchable
            className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-primary-500 text-sm text-typography-0 transition hover:bg-primary-600 disabled:cursor-not-allowed disabled:opacity-40"
            onPress={onSend}
            disabled={!draft.trim()}
            title="Send"
          >
            ➤
          </Touchable>
        )}
      </View>
      {model && (
        <View className="mt-1.5 text-right text-[11px] text-typography-400" title={model}>{model}</View>
      )}
    </View>
  )
}

export default App
