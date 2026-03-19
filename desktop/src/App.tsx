import { useState, useEffect, useRef, useCallback, useMemo } from 'react'
import './App.css'

// ── Tauri integration (no-ops when running in browser) ────────────────────────

const isTauri = () => '__TAURI_INTERNALS__' in window

async function tauriInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const { invoke } = await import('@tauri-apps/api/core')
  return invoke<T>(cmd, args)
}

async function tauriListen<T>(
  event: string,
  handler: (payload: T) => void,
): Promise<() => void> {
  const { listen } = await import('@tauri-apps/api/event')
  return listen<T>(event, e => handler(e.payload))
}

async function tauriPickFolder(): Promise<string | null> {
  const { open } = await import('@tauri-apps/plugin-dialog')
  const result = await open({ directory: true, title: 'Select Repository' })
  return typeof result === 'string' ? result : null
}

// ── Types ──────────────────────────────────────────────────────────────────────

type ServerFrame =
  | { type: 'ready';                session_id: string; resumed: boolean }
  | { type: 'text';                 text: string }
  | { type: 'tool_use';             tool: string; input: Record<string, unknown> }
  | { type: 'tool_result';          tool_use_id: string; content: unknown }
  | { type: 'result';               cost_usd: number; turns: number; session_id: string; result: string | null }
  | { type: 'error';                message: string }
  | { type: 'interrupted' }
  | { type: 'system';               text: string }
  | { type: 'spawning';             task: string }
  | { type: 'worker_created';       branch: string; worktree_path: string; task: string }
  | { type: 'worker_error';         message: string }
  | { type: 'worker_session_ready'; branch: string; worker_session_id: string; task: string }

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
  wsUrl: string          // used in browser mode
  sessionId?: string     // used in Tauri mode
  initialMessage?: string
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
  sessionId?: string     // pre-assigned session ID for Tauri worker panes
  active: boolean
  canSpawnWorker: boolean
  repo: string
  completionRoots: string[]   // dirs to search for @ completions (repo + worktrees)
  initialMessage?: string
  onStatusChange: (status: ConnStatus) => void
  onWorkerCreated: (branch: string, worktreePath: string, task: string, workerSessionId: string) => void
}

function ChatPane({
  wsUrl,
  sessionId: externalSessionId,
  active,
  canSpawnWorker,
  repo,
  completionRoots,
  initialMessage,
  onStatusChange,
  onWorkerCreated,
}: ChatPaneProps) {
  const [messages,     setMessages]     = useState<ChatMessage[]>([])
  const [status,       setStatus]       = useState<ConnStatus>('connecting')
  const [isStreaming,  setIsStreaming]   = useState(false)
  const [input,        setInput]        = useState('')
  const [completions,  setCompletions]  = useState<string[]>([])
  const [compIndex,    setCompIndex]    = useState(0)
  const [compQuery,    setCompQuery]    = useState<{ atPos: number; dirPart: string; filePart: string } | null>(null)

  // Tauri session ID for this pane
  const tauriSessionId              = useRef<string | null>(externalSessionId ?? null)

  const wsRef               = useRef<WebSocket | null>(null)
  const inResponseRef       = useRef(false)
  const messagesEndRef      = useRef<HTMLDivElement>(null)
  const inputRef            = useRef<HTMLTextAreaElement>(null)
  const onStatusChangeRef   = useRef(onStatusChange)
  const onWorkerCreatedRef  = useRef(onWorkerCreated)
  const initialMessageSent  = useRef(false)

  onStatusChangeRef.current  = onStatusChange
  onWorkerCreatedRef.current = onWorkerCreated

  const updateStatus = useCallback((s: ConnStatus) => {
    setStatus(s)
    onStatusChangeRef.current(s)
  }, [])

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

  const handleFrame = useCallback((frame: ServerFrame) => {
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
        break
      case 'worker_session_ready':
        onWorkerCreatedRef.current(frame.branch, '', frame.task, frame.worker_session_id)
        break
      case 'worker_error':
        setMessages(prev => [...prev, {
          id: uid(), role: 'info', streaming: false,
          blocks: [{ kind: 'worker_error', message: frame.message }],
        }])
        break
    }
  }, [appendBlock, completeResponse, ensureAssistantMsg, updateStatus])

  // ── Tauri event path ────────────────────────────────────────────────────────

  useEffect(() => {
    if (!isTauri()) return

    let unlistenFn: (() => void) | null = null
    let mounted = true

    const setup = async () => {
      // If we don't have a session yet, create one
      if (!tauriSessionId.current) {
        const sessionType = wsUrl.includes('/workers/') ? 'worker' : 'main'
        const branch = wsUrl.includes('/workers/')
          ? decodeURIComponent(wsUrl.split('/workers/')[1])
          : undefined
        const sid = await tauriInvoke<string>('chat_new_session', {
          sessionType,
          branch: branch ?? null,
          worktreePath: null,
          repo,
        })
        if (!mounted) return
        tauriSessionId.current = sid
      }

      const sid = tauriSessionId.current!
      unlistenFn = await tauriListen<ServerFrame>(`claude-event-${sid}`, handleFrame)
    }

    setup().catch(() => updateStatus('error'))

    return () => {
      mounted = false
      unlistenFn?.()
    }
  }, [wsUrl, repo]) // stable — handleFrame/updateStatus are stable callbacks via useCallback

  // Send initial message once session is ready (both Tauri and WebSocket modes)
  useEffect(() => {
    if (!initialMessage || initialMessageSent.current) return
    if (status !== 'ready' && status !== 'resumed') return
    initialMessageSent.current = true
    setMessages(prev => [...prev, {
      id: uid(), role: 'user', streaming: false,
      blocks: [{ kind: 'text', text: initialMessage }],
    }])
    if (isTauri()) {
      const sid = tauriSessionId.current
      if (sid) tauriInvoke('chat_send', { sessionId: sid, text: initialMessage })
    } else {
      const ws = wsRef.current
      if (ws && ws.readyState === WebSocket.OPEN)
        ws.send(JSON.stringify({ type: 'message', text: initialMessage }))
    }
  }, [status, initialMessage])

  // ── WebSocket path (browser mode) ──────────────────────────────────────────

  useEffect(() => {
    if (isTauri()) return

    let cancelled = false

    const connect = () => {
      if (cancelled) return
      updateStatus('connecting')
      const ws = new WebSocket(wsUrl)
      wsRef.current = ws

      ws.onmessage = ({ data }) => {
        let frame: ServerFrame
        try { frame = JSON.parse(data) } catch { return }
        handleFrame(frame)
      }

      ws.onclose = () => {
        if (!cancelled) {
          if (inResponseRef.current) {
            inResponseRef.current = false
            setIsStreaming(false)
            setMessages(prev => prev.map((m, i) =>
              i < prev.length - 1 ? m : { ...m, streaming: false }
            ))
          }
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
  }, [wsUrl, appendBlock, completeResponse, ensureAssistantMsg, updateStatus, handleFrame])

  // ── Send / interrupt ────────────────────────────────────────────────────────

  const sendMessage = useCallback(() => {
    const text = input.trim()
    if (!text || isStreaming) return

    if (canSpawnWorker && text.startsWith('&')) {
      const task = text.slice(1).trim()
      if (!task) return
      setMessages(prev => [...prev, {
        id: uid(), role: 'user', streaming: false,
        blocks: [{ kind: 'spawning', task }],
      }])
      if (isTauri()) {
        const sid = tauriSessionId.current
        if (sid) tauriInvoke('spawn_worker', { sessionId: sid, task, repo })
      } else {
        wsRef.current?.send(JSON.stringify({ type: 'spawn_worker', task }))
      }
    } else {
      setMessages(prev => [...prev, {
        id: uid(), role: 'user', streaming: false,
        blocks: [{ kind: 'text', text }],
      }])
      if (isTauri()) {
        const sid = tauriSessionId.current
        if (sid) tauriInvoke('chat_send', { sessionId: sid, text })
      } else {
        const ws = wsRef.current
        if (!ws || ws.readyState !== WebSocket.OPEN) return
        ws.send(JSON.stringify({ type: 'message', text }))
      }
    }

    setInput('')
    inputRef.current?.focus()
  }, [input, isStreaming, canSpawnWorker, repo])

  const sendInterrupt = useCallback(() => {
    if (isTauri()) {
      const sid = tauriSessionId.current
      if (sid) tauriInvoke('chat_interrupt', { sessionId: sid })
    } else {
      wsRef.current?.send(JSON.stringify({ type: 'interrupt' }))
    }
  }, [])

  // Parse `@`-triggered path completion from text before cursor
  const parseAtCompletion = (text: string, cursor: number) => {
    const before = text.slice(0, cursor)
    const atIdx = before.lastIndexOf('@')
    if (atIdx === -1) return null
    const fragment = before.slice(atIdx + 1)
    if (fragment.includes(' ')) return null   // space in fragment → not a path
    const slash = fragment.lastIndexOf('/')
    const dirPart  = slash >= 0 ? fragment.slice(0, slash + 1) : ''
    const filePart = slash >= 0 ? fragment.slice(slash + 1) : fragment
    return { atPos: atIdx, dirPart, filePart }
  }

  const acceptCompletion = useCallback((completion: string) => {
    if (!compQuery) return
    const ta = inputRef.current
    const cursor = ta?.selectionStart ?? input.length
    // Replace from the '@' up to cursor with '@' + completion
    const before = input.slice(0, compQuery.atPos + 1) // keep the '@'
    const after  = input.slice(cursor)
    const next   = before + completion + after
    setInput(next)
    setCompletions([])
    setCompQuery(null)
    // Move cursor to end of inserted completion
    const newCursor = compQuery.atPos + 1 + completion.length
    requestAnimationFrame(() => {
      ta?.setSelectionRange(newCursor, newCursor)
      ta?.focus()
    })
  }, [compQuery, input])

  const handleInputChange = useCallback((e: React.ChangeEvent<HTMLTextAreaElement>) => {
    const val = e.target.value
    setInput(val)

    if (!isTauri()) { setCompletions([]); setCompQuery(null); return }

    const cursor = e.target.selectionStart ?? val.length
    const query = parseAtCompletion(val, cursor)
    if (!query) { setCompletions([]); setCompQuery(null); return }

    setCompQuery(query)
    setCompIndex(0)
    tauriInvoke<string[]>('get_completions', {
      roots: completionRoots.length ? completionRoots : [repo],
      dirPart: query.dirPart,
      filePart: query.filePart,
    }).then(items => {
      setCompletions(items)
      setCompIndex(0)
    }).catch(() => setCompletions([]))
  }, [completionRoots, repo])

  const handleKeyDown = useCallback((e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (completions.length > 0) {
      if (e.key === 'Tab' || e.key === 'ArrowDown') {
        e.preventDefault()
        setCompIndex(i => (i + 1) % completions.length)
        return
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault()
        setCompIndex(i => (i - 1 + completions.length) % completions.length)
        return
      }
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault()
        acceptCompletion(completions[compIndex])
        return
      }
      if (e.key === 'Escape') {
        setCompletions([])
        setCompQuery(null)
        return
      }
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      sendMessage()
    }
  }, [sendMessage, completions, compIndex, acceptCompletion])

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
        {completions.length > 0 && (
          <ul className="completion-list">
            {completions.map((c, i) => (
              <li
                key={c}
                className={`completion-item${i === compIndex ? ' completion-item--active' : ''}`}
                onMouseDown={e => { e.preventDefault(); acceptCompletion(c) }}
                onMouseEnter={() => setCompIndex(i)}
              >
                {c}
              </li>
            ))}
          </ul>
        )}
        <div className="input-row">
          <textarea
            ref={inputRef}
            className="chat-input"
            value={input}
            onChange={handleInputChange}
            onKeyDown={handleKeyDown}
            placeholder={placeholder}
            rows={1}
            disabled={status === 'connecting' || status === 'disconnected'}
          />
          <div className="input-actions">
            {isStreaming
              ? <button className="btn btn--interrupt" onClick={sendInterrupt}>stop</button>
              : <button className="btn btn--send" onClick={sendMessage} disabled={!canSend}>send</button>
            }
            {!isStreaming && messages.length > 0 && (
              <button className="btn btn--clear" onClick={() => setMessages([])}>clear</button>
            )}
          </div>
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
  const [tabs,        setTabs]        = useState<Tab[]>([
    { id: 'main', label: 'main', wsUrl: MAIN_WS_URL },
  ])
  const [activeTab,   setActiveTab]   = useState('main')
  const [tabStatuses, setTabStatuses] = useState<Record<string, ConnStatus>>({ main: 'connecting' })
  const [branches,    setBranches]    = useState<Branch[]>([])
  const [repoPath,    setRepoPath]    = useState<string | null>(null)
  const [repoReady,   setRepoReady]   = useState(!isTauri())
  const [apiKey,      setApiKeyState] = useState<string | null>(null)
  const [apiKeyInput, setApiKeyInput] = useState('')

  // Tauri: load stored repo + API key on mount
  useEffect(() => {
    if (!isTauri()) return
    Promise.all([
      tauriInvoke<string | null>('get_repo'),
      tauriInvoke<string | null>('get_api_key'),
    ]).then(([repo, key]) => {
      if (repo) { setRepoPath(repo); setRepoReady(true) }
      if (key)  setApiKeyState(key)
    })
  }, [])

  const pickRepo = useCallback(async () => {
    const folder = await tauriPickFolder()
    if (!folder) return
    await tauriInvoke('set_repo', { repo: folder })
    setRepoPath(folder)
    setRepoReady(true)
  }, [])

  const saveApiKey = useCallback(async () => {
    const key = apiKeyInput.trim()
    if (!key) return
    await tauriInvoke('set_api_key', { key })
    setApiKeyState(key)
    setApiKeyInput('')
  }, [apiKeyInput])

  // Branch polling — Tauri uses invoke, browser uses HTTP
  useEffect(() => {
    const repo = repoPath
    const fetchBranches = () => {
      if (isTauri() && repo) {
        tauriInvoke<Branch[]>('get_branches', { repo }).then(setBranches).catch(() => {})
      } else if (!isTauri()) {
        fetch(BRANCHES_URL)
          .then(r => r.ok ? r.json() : null)
          .then(d => d && setBranches(d))
          .catch(() => {})
      }
    }
    fetchBranches()
    const t = setInterval(fetchBranches, 10_000)
    return () => clearInterval(t)
  }, [repoPath])

  const openTab = useCallback((branch: string, initialMessage?: string, sessionId?: string) => {
    const wsUrl = `ws://localhost:8000/workers/${encodeURIComponent(branch)}`
    setTabs(prev => {
      if (prev.find(t => t.id === branch)) return prev
      return [...prev, { id: branch, label: branch, wsUrl, sessionId, initialMessage }]
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

  const handleWorkerCreated = useCallback(
    (_branch: string, _worktreePath: string, task: string, workerSessionId: string) => {
      openTab(_branch, task, workerSessionId || undefined)
    },
    [openTab],
  )

  const activeWorktrees = branches.filter(b => b.worktree).length
  const openTabIds = new Set(tabs.map(t => t.id))

  // Completion roots: repo root + all worktree paths that exist
  const completionRoots = useMemo(() => {
    const roots: string[] = []
    if (repoPath) roots.push(repoPath)
    for (const b of branches) {
      if (b.worktree && !roots.includes(b.worktree)) roots.push(b.worktree)
    }
    return roots
  }, [repoPath, branches])

  const statusDotClass = (status: ConnStatus) => {
    if (status === 'ready' || status === 'resumed') return 'tab-dot--ready'
    if (status === 'connecting' || status === 'disconnected') return 'tab-dot--connecting'
    if (status === 'error') return 'tab-dot--error'
    return ''
  }

  const repoName = repoPath ? repoPath.split('/').pop() : null

  // ── Setup screens ──────────────────────────────────────────────────────────

  if (isTauri() && !repoReady) {
    return (
      <div className="app">
        <div className="repo-picker">
          <div className="repo-picker-card">
            <span className="repo-picker-mark">⬡</span>
            <h1 className="repo-picker-title">claudulhu</h1>
            <p className="repo-picker-desc">Select a git repository to manage.</p>
            <button className="repo-picker-btn" onClick={pickRepo}>
              Select Repository
            </button>
          </div>
        </div>
      </div>
    )
  }

  if (isTauri() && !apiKey) {
    return (
      <div className="app">
        <div className="repo-picker">
          <div className="repo-picker-card">
            <span className="repo-picker-mark">⬡</span>
            <h1 className="repo-picker-title">claudulhu</h1>
            <p className="repo-picker-desc">Enter your Anthropic API key to continue.</p>
            <input
              className="api-key-input"
              type="password"
              placeholder="sk-ant-…"
              value={apiKeyInput}
              onChange={e => setApiKeyInput(e.target.value)}
              onKeyDown={e => e.key === 'Enter' && saveApiKey()}
              autoFocus
            />
            <button className="repo-picker-btn" onClick={saveApiKey} disabled={!apiKeyInput.trim()}>
              Save Key
            </button>
          </div>
        </div>
      </div>
    )
  }

  // ── Main UI ────────────────────────────────────────────────────────────────

  return (
    <div className="app">
      <header className="app-header">
        <div className="app-title">
          <span className="title-mark">⬡</span>
          <span>claudulhu</span>
          {repoName && <span className="header-repo">{repoName}</span>}
        </div>
        {isTauri() && (
          <div className="header-actions">
            <button className="btn-change-repo" onClick={pickRepo}>change repo</button>
          </div>
        )}
      </header>

      <main className="app-body">
        {/* ── Chat section ── */}
        <section className="chat-section">
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

          <div className="chat-panes">
            {tabs.map(tab => (
              <div
                key={tab.id}
                className={`chat-pane-wrapper${activeTab === tab.id ? ' chat-pane-wrapper--active' : ''}`}
              >
                <ChatPane
                  wsUrl={tab.wsUrl}
                  sessionId={tab.sessionId}
                  active={activeTab === tab.id}
                  canSpawnWorker={tab.id === 'main'}
                  repo={repoPath ?? ''}
                  completionRoots={completionRoots}
                  initialMessage={tab.initialMessage}
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
