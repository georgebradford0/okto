import { useEffect, useMemo, useRef, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { parseQrPayload, type QrPayload } from './qr'
import {
  encodeClientFrame, parseServerEvent,
  type AgentInfo, type ServerEvent, type TaskRecord,
} from './wire'
import './App.css'

// The pseudo-id we use for lair itself in the sidebar list. Children's ids are
// their names (per AgentInfo.id); 'lair' is reserved so it can never collide.
const LAIR_ID = 'lair'

// Stable empty defaults — important: `itemsByAgent[id] ?? []` would mint a new
// array reference every render, which causes the chat-scroll useEffect to
// fire on every keystroke. Sharing one frozen array keeps reference equality.
const EMPTY_ITEMS:  ChatItem[] = []

// How long to keep the interrupt button locked after the user clicks it,
// before assuming the server's `interrupt_ack` was lost and re-enabling.
// Matches mobile's stopAckTimerRef behavior in mobile/App.tsx.
const STOP_ACK_TIMEOUT_MS = 3000

// How long to keep a task's STOP button latched in "STOPPING" before assuming
// the cancel_task_ack was lost on a WS hiccup. Matches mobile's
// CANCEL_ACK_TIMEOUT_MS.
const CANCEL_ACK_TIMEOUT_MS = 6000

const EMPTY_TASKS: TaskRecord[] = []

// ── Chat item model ──────────────────────────────────────────────────────────
//
// We don't render one row per ServerEvent — that would scroll the user past
// every `text` delta. Instead we fold the stream into the same chat-item shape
// mobile uses: user / assistant / tool / cost / etc. Adjacent `text` events
// from the model accumulate into the *currently streaming* assistant item.

type ChatItem =
  | { kind: 'user';        text: string }
  | { kind: 'assistant';   text: string; done: boolean }
  | { kind: 'tool';        toolUseId: string; tool: string; display: string; outputs: string[]; result?: string }
  | { kind: 'cost';        cost: number; interrupted: boolean }
  | { kind: 'error';       message: string }
  | { kind: 'bg';          text: string }

type ConnStatus = 'ready' | 'streaming' | 'error' | 'pending'

type Status =
  | { kind: 'idle' }
  | { kind: 'connecting'; target: QrPayload }
  | { kind: 'connected';  target: QrPayload; tunnelPort: number; ws: WebSocket }
  | { kind: 'error';      message: string }

function App() {
  const [status, setStatus]       = useState<Status>({ kind: 'idle' })
  const [qrInput, setQrInput]     = useState('')
  const [agents, setAgents]       = useState<AgentInfo[]>([])
  const [activeAgent, setActiveAgent] = useState<string>(LAIR_ID)

  // Per-agent state, keyed by AgentInfo.id (or LAIR_ID). Keeping these
  // separate lets a child's stream keep accumulating while the user is
  // looking at another tab — switching back restores the in-progress
  // transcript, draft, and connection status untouched.
  const [itemsByAgent,      setItemsByAgent]      = useState<Record<string, ChatItem[]>>({})
  const [draftByAgent,      setDraftByAgent]      = useState<Record<string, string>>({})
  const [connStatusByAgent, setConnStatusByAgent] = useState<Record<string, ConnStatus>>({ [LAIR_ID]: 'pending' })
  // stopSent locks the interrupt button at reduced opacity from click until
  // the server's `interrupt_ack` (or our 3 s fallback timer). Mirrors
  // mobile's stopSent/stopAckTimerRef.
  const [stopSentByAgent,   setStopSentByAgent]   = useState<Record<string, boolean>>({})

  // Background-task registry per agent — lair pushes one `tasks` frame on
  // every spawn/completion/cancellation. Mobile lives in mobile/App.tsx as
  // `masterTasks` + per-child `tasks`.
  const [tasksByAgent,      setTasksByAgent]      = useState<Record<string, TaskRecord[]>>({})

  // Optimistic latch for the per-task STOP button. One Set shared across
  // agents — task_ids are server-allocated UUIDs so they don't collide.
  const [cancellingIds,     setCancellingIds]     = useState<Set<string>>(() => new Set())
  const cancelTimersRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map())

  // Visibility for the Background Tasks modal.
  const [showTasksModal,    setShowTasksModal]    = useState(false)

  // Derived per-agent slices for the active tab. EMPTY_ITEMS is a stable
  // reference so [items] dep checks don't fire when an unrelated tab updates.
  const items      = itemsByAgent[activeAgent]      ?? EMPTY_ITEMS
  const draft      = draftByAgent[activeAgent]      ?? ''
  const connStatus = connStatusByAgent[activeAgent] ?? 'pending'
  const stopSent   = stopSentByAgent[activeAgent]   ?? false
  const tasks      = tasksByAgent[activeAgent]      ?? EMPTY_TASKS

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
  // Per-agent fallback timers that re-enable the interrupt button if the
  // server's interrupt_ack never arrives. Keyed by agent id.
  const stopAckTimersRef = useRef<Record<string, ReturnType<typeof setTimeout> | null>>({})

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

  // Apply a chat-stream event to a specific agent's slot. Runs regardless of
  // which tab is currently visible — that's what makes per-agent persistence
  // work; events flow into their own slot and the active tab just renders
  // whichever one is selected.
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

    setItemsByAgent(prev => ({ ...prev, [agentId]: foldEvent(prev[agentId] ?? [], ev) }))
    let next: ConnStatus | null = null
    if (ev.type === 'ready')        next = 'ready'
    if (ev.type === 'text')         next = 'streaming'
    if (ev.type === 'tool_use')     next = 'streaming'
    if (ev.type === 'done')         next = 'ready'
    if (ev.type === 'interrupted')  next = 'ready'
    if (ev.type === 'error')        next = 'error'
    if (next !== null) {
      setConnStatusByAgent(prev => ({ ...prev, [agentId]: next! }))
    }
    // Server acknowledged our interrupt — re-enable the stop button so the
    // user can interrupt the next turn without waiting for the 3s fallback.
    if (ev.type === 'interrupt_ack' || ev.type === 'interrupted' || ev.type === 'done') {
      clearStopLock(agentId)
    }
  }

  // The master WS is special: it always handles `agents` (which only lair
  // emits) regardless of which tab is visible. Its chat events feed lair's
  // slot. Children never push `agents`, so their handler is plain applyChatEvent.
  const handleMasterEvent = (ev: ServerEvent) => {
    if (ev.type === 'agents') {
      setAgents(ev.agents)
      return
    }
    applyChatEvent(LAIR_ID, ev)
  }

  const connect = async () => {
    const target = parseQrPayload(qrInput)
    if (!target) {
      setStatus({ kind: 'error', message: 'Invalid QR payload — expected 2:<host>:<port>:<pubkey>' })
      return
    }
    setStatus({ kind: 'connecting', target })
    // Fresh session — wipe every per-agent slot so nothing leaks across
    // logins to different lairs.
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
    try {
      const tunnelPort = await invoke<number>('noise_connect', {
        host:            target.host,
        port:            target.port,
        serverPubkeyB32: target.pk,
      })
      const ws = new WebSocket(`ws://127.0.0.1:${tunnelPort}/stream`)
      masterWsRef.current = ws
      ws.onopen  = () => setStatus({ kind: 'connected', target, tunnelPort, ws })
      ws.onclose = () => {
        masterWsRef.current = null
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

  const openChildWs = (tunnelPort: number, name: string): WebSocket | null => {
    // If we already have an open or in-flight WS for this child, reuse it
    // — opening a second would just race with the first.
    const existing = childWsRefs.current.get(name)
    if (existing && existing.readyState <= WebSocket.OPEN) return existing

    const ws = new WebSocket(`ws://127.0.0.1:${tunnelPort}/agents/${encodeURIComponent(name)}/stream`)
    childWsRefs.current.set(name, ws)
    ws.onclose = () => {
      if (childWsRefs.current.get(name) === ws) childWsRefs.current.delete(name)
      setConnStatusByAgent(prev => ({ ...prev, [name]: 'pending' }))
    }
    ws.onerror = () => {
      setConnStatusByAgent(prev => ({ ...prev, [name]: 'error' }))
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
      applyChatEvent(name, ev)
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
      openChildWs(status.tunnelPort, id)
    }
  }

  const activeWs = (): WebSocket | null => {
    if (activeAgent === LAIR_ID) return masterWsRef.current
    return childWsRefs.current.get(activeAgent) ?? null
  }

  const send = () => {
    if (status.kind !== 'connected') return
    const text = draft.trim()
    if (!text) return
    const ws = activeWs()
    if (!ws || ws.readyState !== WebSocket.OPEN) return
    ws.send(encodeClientFrame({ type: 'user_message', text }))
    const agentId = activeAgent
    setItemsByAgent(prev => ({
      ...prev,
      [agentId]: [...(prev[agentId] ?? []), { kind: 'user', text }],
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
    setShowTasksModal(false)
    if (status.kind === 'connected') status.ws.close()
    else setStatus({ kind: 'idle' })
  }

  const clearChat = () => {
    if (status.kind !== 'connected') return
    const agentId = activeAgent
    // Wipe the visible log immediately so the click feels instant.
    setItemsByAgent(prev => ({ ...prev, [agentId]: [] }))
    // Ask the server to drop its conversation state too — without this the
    // next message would resume on top of the previous transcript. lair's
    // /clear lives at the root; child clears go through the proxy.
    const base = `http://127.0.0.1:${status.tunnelPort}`
    const url  = agentId === LAIR_ID
      ? `${base}/clear`
      : `${base}/agents/${encodeURIComponent(agentId)}/clear`
    fetch(url, { method: 'POST' }).catch(() => { /* fire-and-forget */ })
  }

  if (status.kind !== 'connected') {
    return (
      <ConnectScreen
        qrInput={qrInput}
        setQrInput={setQrInput}
        onConnect={connect}
        status={status}
      />
    )
  }

  const activeLabel = activeAgent === LAIR_ID
    ? 'Lair'
    : agents.find(a => a.id === activeAgent)?.name ?? activeAgent

  return (
    <div className="shell">
      <Sidebar
        agents={agents}
        activeAgent={activeAgent}
        onSelect={selectAgent}
        onDisconnect={disconnect}
      />
      <div className="main">
        <div className="main-head">
          <span className="main-title">{activeLabel}</span>
          <div className="main-head-right">
            <StatusPill status={connStatus} />
            <button
              className="clear-btn"
              onClick={clearChat}
              disabled={connStatus !== 'ready' || items.length === 0}
              title="Clear chat history"
            >
              Clear
            </button>
            <span className="main-head-spacer" />
            <TasksButton tasks={tasks} onClick={() => setShowTasksModal(v => !v)} />
          </div>
        </div>

        <div className="chat" ref={chatRef}>
          {items.length === 0 && (
            <div className="chat-empty">Awaiting your first message</div>
          )}
          {items.map((item, i) => <Row key={i} item={item} />)}
        </div>

        <InputBar
          draft={draft}
          setDraft={setDraft}
          onSend={send}
          onInterrupt={interrupt}
          streaming={connStatus === 'streaming'}
          stopSent={stopSent}
        />
      </div>

      <TasksDrawer
        visible={showTasksModal}
        agentLabel={activeLabel}
        tasks={tasks}
        cancellingIds={cancellingIds}
        onClose={() => setShowTasksModal(false)}
        onCancel={requestCancelTask}
      />
    </div>
  )
}

// ── Fold ServerEvent into our chat-item list ─────────────────────────────────
//
// Most events become their own item; the exception is `text`, which appends to
// the currently-streaming assistant item (or starts a new one). Tool lifecycle
// (use → output[*] → result) folds into a single `tool` item keyed by id.

function foldEvent(items: ChatItem[], ev: ServerEvent): ChatItem[] {
  switch (ev.type) {
    case 'text': {
      const last = items[items.length - 1]
      if (last && last.kind === 'assistant' && !last.done) {
        const next = items.slice(0, -1)
        next.push({ ...last, text: last.text + ev.text })
        return next
      }
      return [...items, { kind: 'assistant', text: ev.text, done: false }]
    }
    case 'tool_use':
      return [...items, {
        kind:      'tool',
        toolUseId: ev.tool_use_id,
        tool:      ev.tool,
        display:   ev.display ?? humanizeTool(ev.tool),
        outputs:   [],
      }]
    case 'tool_output': {
      // Fold into the most recent matching tool chip.
      const idx = lastIndex(items, x => x.kind === 'tool' && x.toolUseId === ev.tool_use_id)
      if (idx < 0) return items
      const tool = items[idx] as Extract<ChatItem, { kind: 'tool' }>
      const next = items.slice()
      next[idx] = { ...tool, outputs: [...tool.outputs, ev.line] }
      return next
    }
    case 'tool_result': {
      const idx = lastIndex(items, x => x.kind === 'tool' && x.toolUseId === ev.tool_use_id)
      if (idx < 0) return items
      const tool = items[idx] as Extract<ChatItem, { kind: 'tool' }>
      const next = items.slice()
      next[idx] = { ...tool, result: stringifyResult(ev.output) }
      return next
    }
    case 'done':
    case 'interrupted': {
      const next = sealStreamingAssistant(items)
      next.push({ kind: 'cost', cost: ev.cost_usd, interrupted: ev.type === 'interrupted' })
      return next
    }
    case 'interrupt_ack':
      return items
    case 'error':
      return [...sealStreamingAssistant(items), { kind: 'error', message: ev.message }]
    case 'bg_complete':
    case 'bg_progress':
      return [...items, { kind: 'bg', text: ev.text }]
    // ready, system, agents, tasks, cancel_task_ack: not surfaced in the chat log.
    default:
      return items
  }
}

function sealStreamingAssistant(items: ChatItem[]): ChatItem[] {
  const last = items[items.length - 1]
  if (last && last.kind === 'assistant' && !last.done) {
    const next = items.slice(0, -1)
    next.push({ ...last, done: true })
    return next
  }
  return items.slice()
}

function lastIndex<T>(arr: T[], pred: (x: T) => boolean): number {
  for (let i = arr.length - 1; i >= 0; i--) if (pred(arr[i])) return i
  return -1
}

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
    <div className="connect">
      <h1 className="connect-brand">OKTO</h1>
      <div className="connect-rule" />
      <p className="connect-sub">Desktop · v0.1</p>
      <p className="connect-tagline">
        Paste the session QR payload printed by lair on startup.
      </p>
      <textarea
        className="connect-textarea"
        value={qrInput}
        onChange={(e) => setQrInput(e.currentTarget.value)}
        onKeyDown={onKey}
        placeholder="2:1.2.3.4:9000:ABCDEF…"
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
      />
      <button
        className="btn-flat"
        onClick={onConnect}
        disabled={connecting || !qrInput.trim()}
      >
        {connecting ? 'Connecting…' : 'Connect'}
      </button>
      {status.kind === 'error' && (
        <p className="connect-error">{status.message}</p>
      )}
    </div>
  )
}

function Sidebar({
  agents, activeAgent, onSelect, onDisconnect,
}: {
  agents: AgentInfo[]
  activeAgent: string
  onSelect: (id: string) => void
  onDisconnect: () => void
}) {
  return (
    <aside className="sidebar">
      <div className="sidebar-section sidebar-agents sidebar-section-first">
        <ul className="agent-list">
          <AgentRow
            id={LAIR_ID}
            name="Lair"
            statusText="main"
            statusKind="ready"
            active={activeAgent === LAIR_ID}
            onSelect={onSelect}
          />
        </ul>
      </div>

      <div className="sidebar-section sidebar-agents">
        <p className="sidebar-section-title">Agents</p>
        <ul className="agent-list">
          {agents.length === 0 && (
            <li className="agent-empty">No child agents</li>
          )}
          {agents.map(a => (
            <AgentRow
              key={a.id}
              id={a.id}
              name={a.name}
              statusText={a.status}
              statusKind={agentStatusKind(a.status)}
              active={activeAgent === a.id}
              onSelect={onSelect}
            />
          ))}
        </ul>
      </div>

      <div className="sidebar-spacer" />

      <div className="sidebar-foot">
        <button className="btn-ghost danger" onClick={onDisconnect}>
          Disconnect
        </button>
      </div>
    </aside>
  )
}

function AgentRow({
  id, name, statusText, statusKind, active, onSelect,
}: {
  id: string
  name: string
  statusText: string
  statusKind: 'ready' | 'pending' | 'error'
  active: boolean
  onSelect: (id: string) => void
}) {
  return (
    <li>
      <button
        className={`agent-row ${active ? 'active' : ''}`}
        onClick={() => onSelect(id)}
      >
        <span className={`agent-dot dot-${statusKind}`} />
        <span className="agent-name">{name}</span>
        <span className="agent-status">{statusText}</span>
      </button>
    </li>
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
    <button
      className={`tasks-btn ${running > 0 ? 'tasks-btn-active' : ''}`}
      onClick={onClick}
      title="Background tasks"
    >
      <span className={`tasks-btn-dot ${running > 0 ? 'tasks-btn-dot-live' : ''}`} />
      {running > 0 ? `Tasks · ${running}` : 'Tasks'}
    </button>
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
      <div
        className={`tasks-drawer-backdrop ${visible ? 'open' : ''}`}
        onClick={onClose}
      />
      <aside
        className={`tasks-drawer ${visible ? 'open' : ''}`}
        aria-hidden={!visible}
      >
        <div className="tasks-drawer-head">
          <div>
            <div className="tasks-drawer-title">Background Tasks</div>
            <div className="tasks-drawer-sub">{agentLabel}</div>
          </div>
          <button className="tasks-drawer-close" onClick={onClose} title="Close (Esc)">✕</button>
        </div>
        <div className="tasks-drawer-body">
          {sorted.length === 0 ? (
            <div className="tasks-empty">No background tasks</div>
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
        </div>
      </aside>
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
    <div className="task-row">
      <div className="task-row-head">
        <span className={`task-status-tag task-status-${statusKind}`}>
          <span className={`task-status-dot dot-${statusKind}`} />
          <span className="task-status-label">{task.status.toUpperCase()}</span>
        </span>
        {task.wake_interval_secs != null && (
          <span className="task-monitored">◈ MONITORED</span>
        )}
        <span className="task-timestamp">{ts}</span>
        {isRunning && (
          <button
            className={`task-stop-btn ${cancelling ? 'cancelling' : ''}`}
            onClick={onCancel}
            disabled={cancelling}
          >
            {cancelling ? 'Stopping' : 'Stop'}
          </button>
        )}
      </div>
      <button
        className="task-body"
        onClick={() => setExpanded(v => !v)}
        title={expanded ? 'Collapse' : 'Expand'}
      >
        <div className={`task-command ${expanded ? '' : 'task-clamp'}`}>{task.command}</div>
        {task.summary && task.summary.length > 0 && (
          <div className={`task-summary ${expanded ? '' : 'task-clamp'}`}>{task.summary}</div>
        )}
        {task.cost_usd != null && task.cost_usd > 0 && (
          <div className="task-cost">{formatCost(task.cost_usd)}</div>
        )}
      </button>
    </div>
  )
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

function StatusPill({ status }: { status: ConnStatus }) {
  const label = useMemo(() => {
    if (status === 'ready')      return 'Ready'
    if (status === 'streaming')  return 'Streaming'
    if (status === 'error')      return 'Error'
    return 'Connecting'
  }, [status])
  return (
    <span className={`status-pill status-${status}`}>
      <span className={`dot dot-${status}`} />
      <span className="label">{label}</span>
    </span>
  )
}

function Row({ item }: { item: ChatItem }) {
  switch (item.kind) {
    case 'user':
      return (
        <div className="row right">
          <div className="user-bubble">{item.text}</div>
        </div>
      )
    case 'assistant':
      return <div className="row"><div className="assistant-text">{item.text}</div></div>
    case 'tool':
      return (
        <div className="row">
          <div className="tool-chip">
            <span className="tool-line">▸ {item.display}</span>
            {item.outputs.length > 0 && (
              <div className="tool-output">{item.outputs.join('\n')}</div>
            )}
            {item.result !== undefined && item.outputs.length === 0 && (
              <div className="tool-result">{truncate(item.result, 800)}</div>
            )}
          </div>
        </div>
      )
    case 'cost':
      return (
        <div className="row">
          {item.interrupted ? (
            <span className="interrupted-line">● Interrupted · ${item.cost.toFixed(4)}</span>
          ) : (
            <span className="cost-label">${item.cost.toFixed(4)}</span>
          )}
        </div>
      )
    case 'error':
      return <div className="row"><span className="error-line">● {item.message}</span></div>
    case 'bg':
      return <div className="row"><span className="bg-line">{item.text}</span></div>
  }
}

function truncate(s: string, n: number): string {
  if (s.length <= n) return s
  return `${s.slice(0, n)}\n…[${s.length - n} more chars]`
}

function InputBar({
  draft, setDraft, onSend, onInterrupt, streaming, stopSent,
}: {
  draft: string
  setDraft: (s: string) => void
  onSend: () => void
  onInterrupt: () => void
  streaming: boolean
  stopSent: boolean
}) {
  const taRef = useRef<HTMLTextAreaElement>(null)

  // Auto-grow textarea.
  useEffect(() => {
    const ta = taRef.current
    if (!ta) return
    ta.style.height = 'auto'
    ta.style.height = `${Math.min(ta.scrollHeight, 200)}px`
  }, [draft])

  const onKey = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      onSend()
    }
  }

  return (
    <div className="input-bar">
      <div className="input-row">
        <textarea
          ref={taRef}
          className="chat-input"
          value={draft}
          onChange={(e) => setDraft(e.currentTarget.value)}
          onKeyDown={onKey}
          placeholder="Message lair…"
          rows={1}
        />
        {streaming ? (
          // Mirrors mobile's stop-button-with-orbit: the OrbitingArc spins
          // around the red stop button while the model is generating;
          // clicking sends an interrupt and locks the button at reduced
          // opacity until the server's interrupt_ack (or our 3 s fallback).
          <div className={`input-btn-slot ${stopSent ? 'stop-sent' : ''}`}>
            <span className="orbit-arc" />
            <button
              className="stop-btn"
              onClick={onInterrupt}
              disabled={stopSent}
              title={stopSent ? 'Interrupt sent…' : 'Interrupt'}
            >
              <span className="stop-icon" />
            </button>
          </div>
        ) : (
          <button
            className="send-btn"
            onClick={onSend}
            disabled={!draft.trim()}
            title="Send"
          >
            <span className="send-btn-icon">➤</span>
          </button>
        )}
      </div>
    </div>
  )
}

export default App
