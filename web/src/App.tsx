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

type ConnStatus = 'connecting' | 'ready' | 'resumed' | 'error' | 'disconnected'

// ── Helpers ───────────────────────────────────────────────────────────────────

let _id = 0
const uid = () => `m${++_id}`

const WS_URL       = 'ws://localhost:8000/chat'
const BRANCHES_URL = 'http://localhost:8000/branches'

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

// ── BranchItem ────────────────────────────────────────────────────────────────

function BranchItem({ branch }: { branch: Branch }) {
  return (
    <div className="branch-item">
      <span className={`branch-dot${branch.worktree ? ' branch-dot--active' : ''}`} />
      <div className="branch-info">
        <span className="branch-name">{branch.name}</span>
        <span className="branch-commit">{branch.commit}</span>
        {branch.worktree && (
          <span className="branch-worktree">{branch.worktree}</span>
        )}
      </div>
    </div>
  )
}

// ── App ───────────────────────────────────────────────────────────────────────

export default function App() {
  const [messages,    setMessages]   = useState<ChatMessage[]>([])
  const [status,      setStatus]     = useState<ConnStatus>('connecting')
  const [isStreaming, setIsStreaming] = useState(false)
  const [input,       setInput]      = useState('')
  const [branches,    setBranches]   = useState<Branch[]>([])

  const wsRef           = useRef<WebSocket | null>(null)
  const inResponseRef   = useRef(false)
  const messagesEndRef  = useRef<HTMLDivElement>(null)
  const inputRef        = useRef<HTMLTextAreaElement>(null)

  // Auto-scroll
  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages])

  // Auto-resize textarea
  useEffect(() => {
    const ta = inputRef.current
    if (!ta) return
    ta.style.height = 'auto'
    ta.style.height = Math.min(ta.scrollHeight, 120) + 'px'
  }, [input])

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

  // Message helpers (all use functional setMessages to avoid stale closures)
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
      // Merge consecutive text blocks
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
      setStatus('connecting')
      const ws = new WebSocket(WS_URL)
      wsRef.current = ws

      ws.onmessage = ({ data }) => {
        let frame: ServerFrame
        try { frame = JSON.parse(data) } catch { return }

        switch (frame.type) {
          case 'ready':
            setStatus(frame.resumed ? 'resumed' : 'ready')
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
            // Update the last user message (which showed the & task) with spawning status
            break
          case 'worker_created':
            setMessages(prev => [...prev, {
              id: uid(), role: 'info', streaming: false,
              blocks: [{ kind: 'worker_created', branch: frame.branch, worktree_path: frame.worktree_path }],
            }])
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
          setStatus('disconnected')
          setTimeout(connect, 3000)
        }
      }

      ws.onerror = () => setStatus('error')
    }

    connect()
    return () => {
      cancelled = true
      wsRef.current?.close()
    }
  }, [appendBlock, completeResponse, ensureAssistantMsg])

  // Send
  const sendMessage = useCallback(() => {
    const text = input.trim()
    if (!text || isStreaming) return
    const ws = wsRef.current
    if (!ws || ws.readyState !== WebSocket.OPEN) return

    if (text.startsWith('&')) {
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
  }, [input, isStreaming])

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

  const statusLabel: Record<ConnStatus, string> = {
    connecting:   'connecting…',
    ready:        'connected',
    resumed:      'resumed',
    error:        'error',
    disconnected: 'reconnecting…',
  }

  const activeWorktrees = branches.filter(b => b.worktree).length

  return (
    <div className="app">
      <header className="app-header">
        <div className="app-title">
          <span className="title-mark">⬡</span>
          <span>claudulhu</span>
        </div>
        <div className={`status-badge status-badge--${status}`}>
          <span className="status-dot" />
          {statusLabel[status]}
        </div>
      </header>

      <main className="app-body">
        {/* ── Chat ── */}
        <section className="chat-panel">
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
              placeholder="message… (& task to spawn a worktree)"
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
              : branches.map(b => <BranchItem key={b.name} branch={b} />)
            }
          </div>
        </aside>
      </main>
    </div>
  )
}
