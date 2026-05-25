import { useEffect, useMemo, useRef, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { parseQrPayload, type QrPayload } from './qr'
import {
  encodeClientFrame, parseServerEvent,
  type AgentInfo, type ServerEvent,
} from './wire'
import './App.css'

// The pseudo-id we use for lair itself in the sidebar list. Children's ids are
// their names (per AgentInfo.id); 'lair' is reserved so it can never collide.
const LAIR_ID = 'lair'

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
  const [items, setItems]         = useState<ChatItem[]>([])
  const [draft, setDraft]         = useState('')
  const [connStatus, setConnStatus] = useState<ConnStatus>('pending')
  const [agents, setAgents]       = useState<AgentInfo[]>([])
  const [activeAgent, setActiveAgent] = useState<string>(LAIR_ID)

  const chatRef = useRef<HTMLDivElement>(null)
  // Stick to the bottom while the user is at the bottom; let them scroll up.
  const stickToBottomRef = useRef(true)

  // We maintain *two* WebSockets when chatting with a child:
  //
  //   masterWsRef → ws://tunnel/stream            (always-on after connect;
  //                                                feeds the agents list)
  //   childWsRef  → ws://tunnel/agents/<id>/stream (opened on agent select,
  //                                                closed when leaving)
  //
  // When activeAgent === LAIR_ID, the master WS *is* the chat WS — child is
  // null. This mirrors the master/child split in mobile/App.tsx.
  const masterWsRef = useRef<WebSocket | null>(null)
  const childWsRef  = useRef<WebSocket | null>(null)
  // WS message handlers fire from event-loop callbacks and would otherwise
  // close over a stale `activeAgent`. Bounce through a ref so the master's
  // chat-vs-list routing always sees the current selection.
  const activeAgentRef = useRef<string>(LAIR_ID)
  useEffect(() => { activeAgentRef.current = activeAgent }, [activeAgent])

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

  const applyChatEvent = (ev: ServerEvent) => {
    setItems(prev => foldEvent(prev, ev))
    if (ev.type === 'ready')         setConnStatus('ready')
    if (ev.type === 'text')          setConnStatus('streaming')
    if (ev.type === 'tool_use')      setConnStatus('streaming')
    if (ev.type === 'done')          setConnStatus('ready')
    if (ev.type === 'interrupted')   setConnStatus('ready')
    if (ev.type === 'error')         setConnStatus('error')
  }

  // The master WS is special: it always handles `agents` (which only lair
  // emits), but its chat events should only feed the visible chat when Lair
  // is the active agent. Children never push `agents`, so their handler is
  // plain applyChatEvent.
  const handleMasterEvent = (ev: ServerEvent) => {
    if (ev.type === 'agents') {
      setAgents(ev.agents)
      return
    }
    if (activeAgentRef.current === LAIR_ID) applyChatEvent(ev)
  }

  const connect = async () => {
    const target = parseQrPayload(qrInput)
    if (!target) {
      setStatus({ kind: 'error', message: 'Invalid QR payload — expected 2:<host>:<port>:<pubkey>' })
      return
    }
    setStatus({ kind: 'connecting', target })
    setItems([])
    setAgents([])
    setActiveAgent(LAIR_ID)
    activeAgentRef.current = LAIR_ID
    setConnStatus('pending')
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
        setStatus({ kind: 'idle' })
        setConnStatus('pending')
        setAgents([])
      }
      ws.onerror = () => { setStatus({ kind: 'error', message: 'WebSocket error' }); setConnStatus('error') }
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
      setConnStatus('error')
    }
  }

  const openChildWs = (tunnelPort: number, name: string) => {
    const ws = new WebSocket(`ws://127.0.0.1:${tunnelPort}/agents/${encodeURIComponent(name)}/stream`)
    childWsRef.current = ws
    ws.onclose = () => {
      if (childWsRef.current === ws) childWsRef.current = null
      // Only flip status if this child is still the active target; if the
      // user has already switched away, leave their new chat alone.
      if (activeAgentRef.current === name) setConnStatus('pending')
    }
    ws.onerror = () => {
      if (activeAgentRef.current === name) setConnStatus('error')
    }
    ws.onmessage = (e) => {
      const data = typeof e.data === 'string' ? e.data : ''
      const ev = parseServerEvent(data)
      if (!ev) return
      if (ev.type === 'ping') {
        ws.send(encodeClientFrame({ type: 'pong', id: ev.id }))
        return
      }
      if (activeAgentRef.current === name) applyChatEvent(ev)
    }
  }

  const selectAgent = (id: string) => {
    if (status.kind !== 'connected') return
    if (id === activeAgent) return
    // Drop any in-flight child WS — its chat events would otherwise leak
    // into the new selection's chat for a tick after the switch.
    if (childWsRef.current) {
      try { childWsRef.current.close() } catch {}
      childWsRef.current = null
    }
    setItems([])
    setActiveAgent(id)
    activeAgentRef.current = id
    if (id === LAIR_ID) {
      // Master is already open; mirror its current ready/streaming state.
      setConnStatus(masterWsRef.current?.readyState === WebSocket.OPEN ? 'ready' : 'pending')
    } else {
      setConnStatus('pending')
      openChildWs(status.tunnelPort, id)
    }
  }

  const activeWs = (): WebSocket | null => {
    return activeAgent === LAIR_ID ? masterWsRef.current : childWsRef.current
  }

  const send = () => {
    if (status.kind !== 'connected') return
    const text = draft.trim()
    if (!text) return
    const ws = activeWs()
    if (!ws || ws.readyState !== WebSocket.OPEN) return
    ws.send(encodeClientFrame({ type: 'user_message', text }))
    setItems(prev => [...prev, { kind: 'user', text }])
    setDraft('')
    stickToBottomRef.current = true
  }

  const interrupt = () => {
    const ws = activeWs()
    if (!ws || ws.readyState !== WebSocket.OPEN) return
    ws.send(encodeClientFrame({ type: 'interrupt' }))
  }

  const disconnect = () => {
    if (childWsRef.current) { try { childWsRef.current.close() } catch {}; childWsRef.current = null }
    if (status.kind === 'connected') status.ws.close()
    else setStatus({ kind: 'idle' })
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
        connStatus={connStatus}
        onSelect={selectAgent}
        onDisconnect={disconnect}
      />
      <div className="main">
        <div className="main-head">
          <span className="main-title">{activeLabel}</span>
          <StatusPill status={connStatus} />
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
        />
      </div>
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
  agents, activeAgent, connStatus, onSelect, onDisconnect,
}: {
  agents: AgentInfo[]
  activeAgent: string
  connStatus: ConnStatus
  onSelect: (id: string) => void
  onDisconnect: () => void
}) {
  return (
    <aside className="sidebar">
      <div className="sidebar-head">
        <div className="sidebar-brand">OKTO</div>
        <div className="sidebar-brand-sub">Lair · Desktop</div>
      </div>

      <div className="sidebar-section">
        <p className="sidebar-section-title">Status</p>
        <StatusPill status={connStatus} />
      </div>

      <div className="sidebar-section sidebar-agents">
        <p className="sidebar-section-title">Agents</p>
        <ul className="agent-list">
          <AgentRow
            id={LAIR_ID}
            name="Lair"
            statusText="master"
            statusKind="ready"
            active={activeAgent === LAIR_ID}
            onSelect={onSelect}
          />
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
  draft, setDraft, onSend, onInterrupt, streaming,
}: {
  draft: string
  setDraft: (s: string) => void
  onSend: () => void
  onInterrupt: () => void
  streaming: boolean
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
          <button className="stop-btn" onClick={onInterrupt} title="Interrupt">
            <span className="stop-icon" />
          </button>
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
