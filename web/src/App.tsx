import { useState, useEffect, useRef, useCallback } from 'react'
import './App.css'

// ── Types ──────────────────────────────────────────────────────────────────────

type ServerFrame =
  | { type: 'ready';          session_id: string; resumed: boolean }
  | { type: 'text';           text: string }
  | { type: 'tool_use';       tool: string; input: Record<string, unknown> }
  | { type: 'tool_result';    tool_use_id: string; content: unknown }
  | { type: 'result';         cost_usd: number; turns: number; session_id: string; result: string | null }
  | { type: 'error';          message: string }
  | { type: 'interrupted' }
  | { type: 'system';         text: string }
  | { type: 'spawning';       task: string }
  | { type: 'worker_created'; branch: string; worktree_path: string }
  | { type: 'worker_error';   message: string }

type Block =
  | { kind: 'text';           text: string }
  | { kind: 'tool_use';       tool: string; input: Record<string, unknown> }
  | { kind: 'tool_result';    content: unknown }
  | { kind: 'result';         cost_usd: number; turns: number }
  | { kind: 'error';          message: string }
  | { kind: 'interrupted' }
  | { kind: 'system';         text: string }
  | { kind: 'spawning';       task: string }
  | { kind: 'worker_created'; branch: string; worktree_path: string }
  | { kind: 'worker_error';   message: string }

interface ChatMessage {
  id: string
  role: 'user' | 'assistant' | 'info'
  blocks: Block[]
  streaming: boolean
}

interface Branch {
  name: string
  commit: string
  worktree: string | null
}

interface Tab {
  id: string
  label: string
  wsUrl: string
}

type ConnStatus = 'connecting' | 'ready' | 'resumed' | 'error' | 'disconnected'

// ── Helpers ───────────────────────────────────────────────────────────────────

let _id = 0
const uid = () => `m${++_id}`

const BRANCHES_URL = 'http://localhost:8000/branches'
const MAIN_WS_URL  = 'ws://localhost:8000/chat'

// ── ToolUseBlock ──────────────────────────────────────────────────────────────

function ToolUseBlock({ block }: { block: Extract<Block, { kind: 'tool_use' }> }) {
  const [open, setOpen] = useState(false)
  return (
    <div className="tool-block">
      <button className="tool-header" onClick={() => setOpen(o => !o)}>
        <span className="tool-icon">⚙</span>
        <span className="tool-name">{block.tool}</span>
        <span className="tool-toggle">{open ? '▲' : '▼'}</span>
      </button>
      {open && <pre className="tool-body">{JSON.stringify(block.input, null, 2)}</pre>}
    </div>
  )
}

// ── ToolResultBlock ───────────────────────────────────────────────────────────

function ToolResultBlock({ block }: { block: Extract<Block, { kind: 'tool_result' }> }) {
  const [open, setOpen] = useState(false)
  const text = typeof block.content === 'string'
    ? block.content
    : JSON.stringify(block.content, null, 2)
  const preview = text.slice(0, 60).replace(/\n/g, ' ')
  return (
    <div className="tool-result-block">
      <button className="tool-result-header" onClick={() => setOpen(o => !o)}>
        <span className="result-icon">↩</span>
        <span className="result-preview">{preview}{text.length > 60 ? '…' : ''}</span>
        <span className="tool-toggle">{open ? '▲' : '▼'}</span>
      </button>
      {open && <pre className="tool-body">{text}</pre>}
    </div>
  )
}

// ── BlockRenderer ─────────────────────────────────────────────────────────────

function BlockRenderer({ block }: { block: Block }) {
  switch (block.kind) {
    case 'text':
      return <p className="text-block">{block.text}</p>
    case 'tool_use':
      return <ToolUseBlock block={block} />
    case 'tool_result':
      return <ToolResultBlock block={block} />
    case 'result':
      return (
        <div className="result-footer">
          <span>✓ {block.turns} turn{block.turns !== 1 ? 's' : ''}</span>
          <span>${block.cost_usd.toFixed(4)}</span>
        </div>
      )
    case 'error':
      return <div className="error-block">✗ {block.message}</div>
    case 'interrupted':
      return <div className="interrupted-block">— interrupted</div>
    case 'system':
      return <div className="system-block">{block.text}</div>
    case 'spawning':
      return <div className="spawning-block">generating branch for: {block.task}</div>
    case 'worker_created':
      return (
        <div className="worker-created-block">
          <span className="worker-created-icon">⎇</span>
          <div className="worker-created-info">
            <span className="worker-created-branch">{block.branch}</span>
            <span className="worker-created-path">{block.worktree_path}</span>
          </div>
        </div>
      )
    case 'worker_error':
      return <div className="error-block">✗ {block.message}</div>
  }
}

// ── MessageBubble ─────────────────────────────────────────────────────────────

function MessageBubble({ message }: { message: ChatMessage }) {
  return (
    <div className={`message message--${message.role}`}>
      {message.role !== 'info' && (
        <div className="message-label">
          {message.role === 'user' ? 'you' : 'claude'}
        </div>
      )}
      <div className="message-body">
        {message.blocks.map((block, i) => (
          <BlockRenderer key={i} block={block} />
        ))}
        {message.streaming && <span className="cursor">▋</span>}
      </div>
    </div>
  )
}

// ── ChatPane ──────────────────────────────────────────────────────────────────

interface ChatPaneProps {
  wsUrl: string
  active: boolean
  canSpawnWorker: boolean
  onStatusChange: (status: ConnStatus) => void
  onWorkerCreated: (branch: string, worktreePath: string) => void
}

function ChatPane({
  wsUrl,
  active,
  canSpawnWorker,
  onStatusChange,
  onWorkerCreated,
}: ChatPaneProps) {
  const [messages,    setMessages]   = useState<ChatMessage[]>([])
  const [status,      setStatus]     = useState<ConnStatus>('connecting')
  const [isStreaming, setIsStreaming] = useState(false)
  const [input,       setInput]      = useState('')

  const wsRef               = useRef<WebSocket | null>(null)
  const inResponseRef       = useRef(false)
  const messagesEndRef      = useRef<HTMLDivElement>(null)
  const inputRef            = useRef<HTMLTextAreaElement>(null)
  const onStatusChangeRef   = useRef(onStatusChange)
  const onWorkerCreatedRef  = useRef(onWorkerCreated)

  // Keep refs current without triggering effect re-runs
  onStatusChangeRef.current  = onStatusChange
  onWorkerCreatedRef.current = onWorkerCreated

  const updateStatus = useCallback((s: ConnStatus) => {
    setStatus(s)
    onStatusChangeRef.current(s)
  }, []) // stable — uses ref internally

  // Auto-scroll
  useEffect(() => {
    if (active) messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages, active])

  // Focus input when tab becomes active
  useEffect(() => {
    if (active) inputRef.current?.focus()
  }, [active])

  // Auto-resize textarea
  useEffect(() => {
    const ta = inputRef.current
    if (!ta) return
    ta.style.height = 'auto'
    ta.style.height = Math.min(ta.scrollHeight, 120) + 'px'
  }, [input])

  const ensureAssistantMsg = useCallback(() => {
    if (!inResponseRef.current) {
      inResponseRef.current = true
      setIsStreaming(true)
      setMessages(prev => [...prev, { id: uid(), role: 'assistant', blocks: [], streaming: true }])
    }
  }, [])

  const appendBlock = useCallback((block: Block) => {
    setMessages(prev => {
      const last = prev[prev.length - 1]
      if (!last?.streaming) return prev
      if (block.kind === 'text') {
        const tail = last.blocks[last.blocks.length - 1]
        if (tail?.kind === 'text') {
          return prev.map((m, i) => i < prev.length - 1 ? m : {
            ...m,
            blocks: [...m.blocks.slice(0, -1), { kind: 'text' as const, text: tail.text + block.text }],
          })
        }
      }
      return prev.map((m, i) => i < prev.length - 1 ? m : { ...m, blocks: [...m.blocks, block] })
    })
  }, [])

  const completeResponse = useCallback(() => {
    inResponseRef.current = false
    setIsStreaming(false)
    setMessages(prev => prev.map((m, i) => i < prev.length - 1 ? m : { ...m, streaming: false }))
  }, [])

  // WebSocket
  useEffect(() => {
    let cancelled = false

    const connect = () => {
      if (cancelled) return
      updateStatus('connecting')
      const ws = new WebSocket(wsUrl)
      wsRef.current = ws

      ws.onmessage = ({ data }) => {
        let frame: ServerFrame
        try { frame = JSON.parse(data) } catch { return }

        switch (frame.type) {
          case 'ready':
            updateStatus(frame.resumed ? 'resumed' : 'ready')
            break
          case 'text':
            ensureAssistantMsg()
            appendBlock({ kind: 'text', text: frame.text })
            break
          case 'tool_use':
            ensureAssistantMsg()
            appendBlock({ kind: 'tool_use', tool: frame.tool, input: frame.input })
            break
          case 'tool_result':
            appendBlock({ kind: 'tool_result', content: frame.content })
            break
          case 'result':
            appendBlock({ kind: 'result', cost_usd: frame.cost_usd, turns: frame.turns })
            completeResponse()
            break
          case 'error':
            ensureAssistantMsg()
            appendBlock({ kind: 'error', message: frame.message })
            completeResponse()
            break
          case 'interrupted':
            appendBlock({ kind: 'interrupted' })
            completeResponse()
            break
          case 'system':
            setMessages(prev => [...prev, {
              id: uid(), role: 'info', streaming: false,
              blocks: [{ kind: 'system', text: frame.text }],
            }])
            break
          case 'spawning':
            break
          case 'worker_created':
            setMessages(prev => [...prev, {
              id: uid(), role: 'info', streaming: false,
              blocks: [{ kind: 'worker_created', branch: frame.branch, worktree_path: frame.worktree_path }],
            }])
            onWorkerCreatedRef.current(frame.branch, frame.worktree_path)
            break
          case 'worker_error':
            setMessages(prev => [...prev, {
              id: uid(), role: 'info', streaming: false,
              blocks: [{ kind: 'worker_error', message: frame.message }],
            }])
            break
        }
      }

      ws.onclose = () => {
        if (!cancelled) {
          updateStatus('disconnected')
          setTimeout(connect, 3000)
        }
      }

      ws.onerror = () => updateStatus('error')
    }

    connect()
    return () => {
      cancelled = true
      wsRef.current?.close()
    }
  }, [wsUrl, appendBlock, completeResponse, ensureAssistantMsg, updateStatus])

  const sendMessage = useCallback(() => {
    const text = input.trim()
    if (!text || isStreaming) return
    const ws = wsRef.current
    if (!ws || ws.readyState !== WebSocket.OPEN) return

    if (canSpawnWorker && text.startsWith('&')) {
      const task = text.slice(1).trim()
      if (!task) return
      setMessages(prev => [...prev, {
        id: uid(), role: 'user', streaming: false,
        blocks: [{ kind: 'spawning', task }],
      }])
      ws.send(JSON.stringify({ type: 'spawn_worker', task }))
    } else {
      setMessages(prev => [...prev, {
        id: uid(), role: 'user', streaming: false,
        blocks: [{ kind: 'text', text }],
      }])
      ws.send(JSON.stringify({ type: 'message', text }))
    }

    setInput('')
    inputRef.current?.focus()
  }, [input, isStreaming, canSpawnWorker])

  const sendInterrupt = useCallback(() => {
    wsRef.current?.send(JSON.stringify({ type: 'interrupt' }))
  }, [])

  const handleKeyDown = useCallback((e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      sendMessage()
    }
  }, [sendMessage])

  const canSend = !isStreaming && !!input.trim() && (status === 'ready' || status === 'resumed')
  const placeholder = canSpawnWorker
    ? 'message… (& task to spawn a worktree)'
    : 'message…'

  return (
    <div className="chat-pane">
      <div className="messages-scroll">
        <div className="messages-inner">
          {messages.length === 0 && (
            <div className="empty-state">
              {status === 'connecting' || status === 'disconnected'
                ? 'establishing connection…'
                : 'send a message to begin'}
            </div>
          )}
          {messages.map(msg => (
            <MessageBubble key={msg.id} message={msg} />
          ))}
          <div ref={messagesEndRef} />
        </div>
      </div>

      <div className="input-area">
        <textarea
          ref={inputRef}
          className="chat-input"
          value={input}
          onChange={e => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder={placeholder}
          rows={1}
          disabled={status === 'connecting' || status === 'disconnected'}
        />
        <div>
          {isStreaming
            ? <button className="btn btn--interrupt" onClick={sendInterrupt}>stop</button>
            : <button className="btn btn--send" onClick={sendMessage} disabled={!canSend}>send</button>
          }
        </div>
      </div>
    </div>
  )
}

// ── BranchItem ────────────────────────────────────────────────────────────────

function BranchItem({
  branch,
  isOpen,
  onOpen,
}: {
  branch: Branch
  isOpen: boolean
  onOpen: () => void
}) {
  return (
    <div
      className={`branch-item${branch.worktree ? ' branch-item--clickable' : ''}${isOpen ? ' branch-item--open' : ''}`}
      onClick={branch.worktree ? onOpen : undefined}
    >
      <span className={`branch-dot${branch.worktree ? ' branch-dot--active' : ''}`} />
      <div className="branch-info">
        <span className="branch-name">{branch.name}</span>
        <span className="branch-commit">{branch.commit}</span>
        {branch.worktree && (
          <span className="branch-worktree">{branch.worktree}</span>
        )}
      </div>
      {branch.worktree && (
        <span className="branch-open-hint">{isOpen ? 'open' : 'chat'}</span>
      )}
    </div>
  )
}

// ── App ───────────────────────────────────────────────────────────────────────

export default function App() {
  const [tabs,       setTabs]       = useState<Tab[]>([
    { id: 'main', label: 'main', wsUrl: MAIN_WS_URL },
  ])
  const [activeTab,  setActiveTab]  = useState('main')
  const [tabStatuses, setTabStatuses] = useState<Record<string, ConnStatus>>({ main: 'connecting' })
  const [branches,   setBranches]   = useState<Branch[]>([])

  // Branch polling
  useEffect(() => {
    const fetch_ = () =>
      fetch(BRANCHES_URL)
        .then(r => r.ok ? r.json() : null)
        .then(d => d && setBranches(d))
        .catch(() => {})
    fetch_()
    const t = setInterval(fetch_, 10_000)
    return () => clearInterval(t)
  }, [])

  const openTab = useCallback((branch: string) => {
    const wsUrl = `ws://localhost:8000/workers/${encodeURIComponent(branch)}`
    setTabs(prev => {
      if (prev.find(t => t.id === branch)) return prev
      return [...prev, { id: branch, label: branch, wsUrl }]
    })
    setTabStatuses(prev => ({ ...prev, [branch]: prev[branch] ?? 'connecting' }))
    setActiveTab(branch)
  }, [])

  const closeTab = useCallback((id: string) => {
    if (id === 'main') return
    setTabs(prev => prev.filter(t => t.id !== id))
    setTabStatuses(prev => { const n = { ...prev }; delete n[id]; return n })
    setActiveTab(prev => prev === id ? 'main' : prev)
  }, [])

  const handleStatusChange = useCallback((id: string) => (status: ConnStatus) => {
    setTabStatuses(prev => ({ ...prev, [id]: status }))
  }, [])

  const handleWorkerCreated = useCallback((branch: string) => {
    openTab(branch)
  }, [openTab])

  const activeWorktrees = branches.filter(b => b.worktree).length
  const openTabIds = new Set(tabs.map(t => t.id))

  const statusDotClass = (status: ConnStatus) => {
    if (status === 'ready' || status === 'resumed') return 'tab-dot--ready'
    if (status === 'connecting' || status === 'disconnected') return 'tab-dot--connecting'
    if (status === 'error') return 'tab-dot--error'
    return ''
  }

  return (
    <div className="app">
      <header className="app-header">
        <div className="app-title">
          <span className="title-mark">⬡</span>
          <span>claudulhu</span>
        </div>
      </header>

      <main className="app-body">
        {/* ── Chat section ── */}
        <section className="chat-section">
          {/* Tab bar */}
          <div className="tab-bar">
            {tabs.map(tab => (
              <div
                key={tab.id}
                className={`tab${activeTab === tab.id ? ' tab--active' : ''}`}
                onClick={() => setActiveTab(tab.id)}
              >
                <span className={`tab-dot ${statusDotClass(tabStatuses[tab.id] ?? 'connecting')}`} />
                <span className="tab-label">{tab.label}</span>
                {tab.id !== 'main' && (
                  <button
                    className="tab-close"
                    onClick={e => { e.stopPropagation(); closeTab(tab.id) }}
                  >×</button>
                )}
              </div>
            ))}
          </div>

          {/* Chat panes — all mounted, only active one visible */}
          <div className="chat-panes">
            {tabs.map(tab => (
              <div
                key={tab.id}
                className={`chat-pane-wrapper${activeTab === tab.id ? ' chat-pane-wrapper--active' : ''}`}
              >
                <ChatPane
                  wsUrl={tab.wsUrl}
                  active={activeTab === tab.id}
                  canSpawnWorker={tab.id === 'main'}
                  onStatusChange={handleStatusChange(tab.id)}
                  onWorkerCreated={handleWorkerCreated}
                />
              </div>
            ))}
          </div>
        </section>

        {/* ── Branches ── */}
        <aside className="branches-panel">
          <div className="panel-header">
            <span className="panel-title">worktrees</span>
            <span className="panel-count">{activeWorktrees}/{branches.length}</span>
          </div>
          <div className="branches-list">
            {branches.length === 0
              ? <div className="branches-empty">no branches found</div>
              : branches.map(b => (
                  <BranchItem
                    key={b.name}
                    branch={b}
                    isOpen={openTabIds.has(b.name)}
                    onOpen={() => openTab(b.name)}
                  />
                ))
            }
          </div>
        </aside>
      </main>
    </div>
  )
}
