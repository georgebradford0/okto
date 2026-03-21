import AsyncStorage from '@react-native-async-storage/async-storage'
import { StatusBar } from 'expo-status-bar'
import { useCallback, useEffect, useRef, useState } from 'react'
import {
  FlatList,
  KeyboardAvoidingView,
  Modal,
  Platform,
  Pressable,
  SafeAreaView,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  TouchableOpacity,
  View,
} from 'react-native'

// ── Types ──────────────────────────────────────────────────────────────────────

type ServerFrame =
  | { type: 'ready';                session_id: string; resumed: boolean }
  | { type: 'text';                 text: string }
  | { type: 'tool_use';             tool: string; input: Record<string, unknown> }
  | { type: 'tool_result';          tool_use_id: string; content: unknown }
  | { type: 'result';               cost_usd: number; turns: number; session_id: string; result: string | null }
  | { type: 'error';                message: string }
  | { type: 'interrupted' }
  | { type: 'question';             question: string }
  | { type: 'system';               text: string }
  | { type: 'spawning';             task: string }
  | { type: 'worker_created';       branch: string; worktree_path: string; task: string }
  | { type: 'worker_error';         message: string }
  | { type: 'worker_session_ready'; branch: string; worktree_path: string; worker_session_id: string; task: string }

type Block =
  | { kind: 'text';           text: string }
  | { kind: 'tool_use';       tool: string; input: Record<string, unknown> }
  | { kind: 'tool_result';    content: unknown }
  | { kind: 'result';         cost_usd: number; turns: number }
  | { kind: 'error';          message: string }
  | { kind: 'interrupted' }
  | { kind: 'question';       question: string }
  | { kind: 'system';         text: string }
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
  worktreePath?: string
  initialMessage?: string
}

type ConnStatus = 'connecting' | 'ready' | 'resumed' | 'error' | 'disconnected'

// ── Helpers ────────────────────────────────────────────────────────────────────

let _id = 0
const uid = () => `m${++_id}`

// ── Colours & theme ────────────────────────────────────────────────────────────

const C = {
  bg:           '#0f0f0f',
  surface:      '#1a1a1a',
  surfaceAlt:   '#222222',
  border:       '#2a2a2a',
  borderLight:  '#333333',
  accent:       '#4f8ef7',
  accentDim:    '#2a4a80',
  green:        '#4caf7d',
  yellow:       '#e8b84b',
  red:          '#e05a5a',
  textPrimary:  '#e8e8e8',
  textSecondary:'#888888',
  textMuted:    '#555555',
  userBubble:   '#1e3a5f',
  userBorder:   '#2a5090',
  asstBubble:   '#1a1a1a',
  asstBorder:   '#2a2a2a',
  infoBubble:   '#161616',
  toolBg:       '#141414',
  toolBorder:   '#2a2a2a',
  inputBg:      '#1a1a1a',
  inputBorder:  '#333333',
}

// ── ToolUseBlock ──────────────────────────────────────────────────────────────

function ToolUseBlock({ tool, input }: { tool: string; input: Record<string, unknown> }) {
  const [open, setOpen] = useState(false)
  return (
    <View style={s.toolBlock}>
      <TouchableOpacity style={s.toolHeader} onPress={() => setOpen(o => !o)} activeOpacity={0.7}>
        <Text style={s.toolIcon}>⚙</Text>
        <Text style={s.toolName}>{tool}</Text>
        <Text style={s.toolToggle}>{open ? '▲' : '▼'}</Text>
      </TouchableOpacity>
      {open && (
        <ScrollView horizontal style={s.toolBody}>
          <Text style={s.toolBodyText}>{JSON.stringify(input, null, 2)}</Text>
        </ScrollView>
      )}
    </View>
  )
}

// ── ToolResultBlock ────────────────────────────────────────────────────────────

function ToolResultBlock({ content }: { content: unknown }) {
  const [open, setOpen] = useState(false)
  const text = typeof content === 'string' ? content : JSON.stringify(content, null, 2)
  const preview = text.slice(0, 55).replace(/\n/g, ' ')
  return (
    <View style={s.toolResultBlock}>
      <TouchableOpacity style={s.toolResultHeader} onPress={() => setOpen(o => !o)} activeOpacity={0.7}>
        <Text style={s.resultIcon}>↩</Text>
        <Text style={s.resultPreview} numberOfLines={1}>
          {preview}{text.length > 55 ? '…' : ''}
        </Text>
        <Text style={s.toolToggle}>{open ? '▲' : '▼'}</Text>
      </TouchableOpacity>
      {open && (
        <ScrollView horizontal style={s.toolBody}>
          <Text style={s.toolBodyText}>{text}</Text>
        </ScrollView>
      )}
    </View>
  )
}

// ── BlockRenderer ─────────────────────────────────────────────────────────────

function BlockRenderer({ block }: { block: Block }) {
  switch (block.kind) {
    case 'text':
      return <Text style={s.textBlock}>{block.text}</Text>
    case 'tool_use':
      return <ToolUseBlock tool={block.tool} input={block.input} />
    case 'tool_result':
      return <ToolResultBlock content={block.content} />
    case 'result':
      return (
        <View style={s.resultFooter}>
          <Text style={s.resultFooterText}>✓ {block.turns} turn{block.turns !== 1 ? 's' : ''}</Text>
          <Text style={s.resultFooterText}>${block.cost_usd.toFixed(4)}</Text>
        </View>
      )
    case 'error':
      return <Text style={s.errorBlock}>✗ {block.message}</Text>
    case 'interrupted':
      return <Text style={s.interruptedBlock}>— interrupted</Text>
    case 'question':
      return (
        <View style={s.questionBlock}>
          <Text style={s.questionIcon}>?</Text>
          <Text style={s.questionText}>{block.question}</Text>
        </View>
      )
    case 'system':
      return <Text style={s.systemBlock}>{block.text}</Text>
    case 'worker_created':
      return (
        <View style={s.workerCreatedBlock}>
          <Text style={s.workerCreatedIcon}>⎇</Text>
          <View>
            <Text style={s.workerCreatedBranch}>{block.branch}</Text>
            <Text style={s.workerCreatedPath}>{block.worktree_path}</Text>
          </View>
        </View>
      )
    case 'worker_error':
      return <Text style={s.errorBlock}>✗ {block.message}</Text>
  }
}

// ── MessageBubble ─────────────────────────────────────────────────────────────

function MessageBubble({ message }: { message: ChatMessage }) {
  const bubbleStyle = message.role === 'user'
    ? s.userBubble
    : message.role === 'info'
      ? s.infoBubble
      : s.asstBubble

  return (
    <View style={[s.messagePadding, message.role === 'user' && s.messageRight]}>
      {message.role !== 'info' && (
        <Text style={[s.messageLabel, message.role === 'user' && s.messageLabelUser]}>
          {message.role === 'user' ? 'you' : 'claude'}
        </Text>
      )}
      <View style={[s.bubble, bubbleStyle]}>
        {message.blocks.map((block, i) => (
          <BlockRenderer key={i} block={block} />
        ))}
        {message.streaming && <Text style={s.cursor}>▋</Text>}
      </View>
    </View>
  )
}

// ── ChatPane ──────────────────────────────────────────────────────────────────

interface ChatPaneProps {
  wsUrl: string
  active: boolean
  canSpawnWorker: boolean
  onStatusChange: (status: ConnStatus) => void
  onWorkerCreated: (branch: string, worktreePath: string, task: string) => void
  initialMessage?: string
}

function ChatPane({ wsUrl, active, canSpawnWorker, onStatusChange, onWorkerCreated, initialMessage }: ChatPaneProps) {
  const [messages,        setMessages]        = useState<ChatMessage[]>([])
  const [status,          setStatus]          = useState<ConnStatus>('connecting')
  const [isStreaming,     setIsStreaming]      = useState(false)
  const [isPending,       setIsPending]       = useState(false)
  const [pendingQuestion, setPendingQuestion] = useState(false)
  const [input,           setInput]           = useState('')

  const wsRef              = useRef<WebSocket | null>(null)
  const inResponseRef      = useRef(false)
  const scrollRef          = useRef<ScrollView>(null)
  const initialMessageSent = useRef(false)
  const onStatusChangeRef  = useRef(onStatusChange)
  const onWorkerCreatedRef = useRef(onWorkerCreated)

  onStatusChangeRef.current  = onStatusChange
  onWorkerCreatedRef.current = onWorkerCreated

  const updateStatus = useCallback((s: ConnStatus) => {
    setStatus(s)
    onStatusChangeRef.current(s)
  }, [])

  const ensureAssistantMsg = useCallback(() => {
    if (!inResponseRef.current) {
      inResponseRef.current = true
      setIsPending(false)
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
    setIsPending(false)
    setIsStreaming(false)
    setPendingQuestion(false)
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
        inResponseRef.current = false
        setIsStreaming(false)
        setIsPending(true)
        setMessages(prev => prev.map((m, i) => i < prev.length - 1 ? m : { ...m, streaming: false }))
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
      case 'question':
        ensureAssistantMsg()
        appendBlock({ kind: 'question', question: frame.question })
        inResponseRef.current = false
        setIsStreaming(false)
        setIsPending(false)
        setMessages(prev => prev.map((m, i) => i < prev.length - 1 ? m : { ...m, streaming: false }))
        setPendingQuestion(true)
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
        onWorkerCreatedRef.current(frame.branch, frame.worktree_path, frame.task)
        completeResponse()
        break
      case 'worker_error':
        setMessages(prev => [...prev, {
          id: uid(), role: 'info', streaming: false,
          blocks: [{ kind: 'worker_error', message: frame.message }],
        }])
        completeResponse()
        break
    }
  }, [appendBlock, completeResponse, ensureAssistantMsg, updateStatus])

  // WebSocket connection
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
        handleFrame(frame)
      }

      ws.onopen = () => {
        // status set by 'ready' frame from server
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
  }, [wsUrl, handleFrame, updateStatus])

  // Send initial message once ready
  useEffect(() => {
    if (!initialMessage || initialMessageSent.current) return
    if (status !== 'ready' && status !== 'resumed') return
    initialMessageSent.current = true
    setMessages(prev => [...prev, {
      id: uid(), role: 'user', streaming: false,
      blocks: [{ kind: 'text', text: initialMessage }],
    }])
    wsRef.current?.send(JSON.stringify({ type: 'message', text: initialMessage }))
  }, [status, initialMessage])

  const sendMessage = useCallback(() => {
    const text = input.trim()
    if (!text) return
    if (isStreaming && !pendingQuestion) return

    if (pendingQuestion) {
      setMessages(prev => [...prev, {
        id: uid(), role: 'user', streaming: false,
        blocks: [{ kind: 'text', text }],
      }])
      wsRef.current?.send(JSON.stringify({ type: 'answer', answer: text }))
      setPendingQuestion(false)
      setIsPending(true)
      setInput('')
      return
    }

    if (canSpawnWorker && text.startsWith('&')) {
      const task = text.slice(1).trim()
      if (!task) return
      setMessages(prev => [...prev, {
        id: uid(), role: 'user', streaming: false,
        blocks: [{ kind: 'system', text: `spawning: ${task}` }],
      }])
      wsRef.current?.send(JSON.stringify({ type: 'spawn_worker', task }))
    } else {
      setMessages(prev => [...prev, {
        id: uid(), role: 'user', streaming: false,
        blocks: [{ kind: 'text', text }],
      }])
      wsRef.current?.send(JSON.stringify({ type: 'message', text }))
    }

    setIsPending(true)
    setInput('')
  }, [input, isStreaming, pendingQuestion, canSpawnWorker])

  const sendInterrupt = useCallback(() => {
    wsRef.current?.send(JSON.stringify({ type: 'interrupt' }))
  }, [])

  const canSend = !!input.trim() && (pendingQuestion || (!isStreaming && (status === 'ready' || status === 'resumed')))
  const inputPlaceholder = pendingQuestion
    ? 'your answer…'
    : canSpawnWorker
      ? 'message… (& task to spawn worktree)'
      : 'message…'

  return (
    <View style={s.chatPane}>
      <ScrollView
        ref={scrollRef}
        style={s.messageScroll}
        contentContainerStyle={s.messageScrollContent}
        onContentSizeChange={() => scrollRef.current?.scrollToEnd({ animated: true })}
      >
        {messages.length === 0 && (
          <Text style={s.emptyState}>
            {status === 'connecting' || status === 'disconnected'
              ? 'connecting to server…'
              : 'send a message to begin'}
          </Text>
        )}
        {messages.map(msg => <MessageBubble key={msg.id} message={msg} />)}
        {isPending && (
          <View style={[s.messagePadding]}>
            <Text style={s.messageLabel}>claude</Text>
            <View style={[s.bubble, s.asstBubble]}>
              <Text style={s.thinkingDots}>• • •</Text>
            </View>
          </View>
        )}
      </ScrollView>

      <View style={s.inputArea}>
        <TextInput
          style={s.textInput}
          value={input}
          onChangeText={setInput}
          placeholder={inputPlaceholder}
          placeholderTextColor={C.textMuted}
          multiline
          maxLength={8000}
          editable={pendingQuestion || status === 'ready' || status === 'resumed'}
          returnKeyType="default"
        />
        <View style={s.inputButtons}>
          {isStreaming ? (
            <TouchableOpacity style={s.btnInterrupt} onPress={sendInterrupt}>
              <Text style={s.btnInterruptText}>stop</Text>
            </TouchableOpacity>
          ) : (
            <TouchableOpacity style={[s.btnSend, !canSend && s.btnDisabled]} onPress={sendMessage} disabled={!canSend}>
              <Text style={[s.btnSendText, !canSend && s.btnDisabledText]}>▶</Text>
            </TouchableOpacity>
          )}
        </View>
      </View>
    </View>
  )
}

// ── App ───────────────────────────────────────────────────────────────────────

export default function App() {
  const [serverUrl,    setServerUrl]    = useState('')
  const [urlInput,     setUrlInput]     = useState('')
  const [isSetup,      setIsSetup]      = useState(false)
  const [tabs,         setTabs]         = useState<Tab[]>([])
  const [activeTab,    setActiveTab]    = useState('main')
  const [tabStatuses,  setTabStatuses]  = useState<Record<string, ConnStatus>>({ main: 'connecting' })
  const [branches,     setBranches]     = useState<Branch[]>([])
  const [showBranches, setShowBranches] = useState(false)

  // Load saved server URL
  useEffect(() => {
    AsyncStorage.getItem('serverUrl').then(url => {
      if (url) {
        setServerUrl(url)
        setUrlInput(url)
        setIsSetup(true)
      }
    })
  }, [])

  // Initialise tab list when serverUrl is set
  useEffect(() => {
    if (!serverUrl) return
    const wsBase = serverUrl.startsWith('ws') ? serverUrl : `ws://${serverUrl}`
    setTabs([{ id: 'main', label: 'main', wsUrl: `${wsBase}/chat` }])
    setActiveTab('main')
    setTabStatuses({ main: 'connecting' })
  }, [serverUrl])

  // Poll branches
  useEffect(() => {
    if (!serverUrl) return
    const httpBase = serverUrl.startsWith('http') ? serverUrl : `http://${serverUrl}`
    const fetch_ = () => {
      fetch(`${httpBase}/branches`)
        .then(r => r.ok ? r.json() : null)
        .then(d => d && setBranches(d))
        .catch(() => {})
    }
    fetch_()
    const t = setInterval(fetch_, 10_000)
    return () => clearInterval(t)
  }, [serverUrl])

  // Auto-close tabs whose worktree is gone
  useEffect(() => {
    setTabs(prev => {
      const toClose = prev.filter(t => {
        if (t.id === 'main') return false
        const b = branches.find(b => b.name === t.id)
        return !b || !b.worktree
      })
      if (toClose.length === 0) return prev
      const closeIds = new Set(toClose.map(t => t.id))
      setTabStatuses(s => { const n = { ...s }; closeIds.forEach(id => delete n[id]); return n })
      setActiveTab(cur => closeIds.has(cur) ? 'main' : cur)
      return prev.filter(t => !closeIds.has(t.id))
    })
  }, [branches])

  const saveUrl = () => {
    const url = urlInput.trim().replace(/\/$/, '')
    if (!url) return
    AsyncStorage.setItem('serverUrl', url)
    setServerUrl(url)
    setIsSetup(true)
  }

  const openTab = useCallback((branch: string, worktreePath?: string, initialMessage?: string) => {
    const wsBase = serverUrl.startsWith('ws') ? serverUrl : `ws://${serverUrl}`
    setTabs(prev => {
      if (prev.find(t => t.id === branch)) return prev
      return [...prev, {
        id: branch,
        label: branch,
        wsUrl: `${wsBase}/workers/${encodeURIComponent(branch)}`,
        worktreePath,
        initialMessage,
      }]
    })
    setTabStatuses(prev => ({ ...prev, [branch]: prev[branch] ?? 'connecting' }))
    setActiveTab(branch)
  }, [serverUrl])

  const closeTab = useCallback((id: string) => {
    if (id === 'main') return
    setTabs(prev => prev.filter(t => t.id !== id))
    setTabStatuses(prev => { const n = { ...prev }; delete n[id]; return n })
    setActiveTab(prev => prev === id ? 'main' : prev)
  }, [])

  const handleStatusChange = useCallback((id: string) => (status: ConnStatus) => {
    setTabStatuses(prev => ({ ...prev, [id]: status }))
  }, [])

  const handleWorkerCreated = useCallback((branch: string, worktreePath: string, task: string) => {
    openTab(branch, worktreePath, task)
  }, [openTab])

  const activeWorktrees = branches.filter(b => b.worktree).length

  const statusColor = (status: ConnStatus) => {
    if (status === 'ready' || status === 'resumed') return C.green
    if (status === 'connecting' || status === 'disconnected') return C.yellow
    return C.red
  }

  // ── Setup screen ────────────────────────────────────────────────────────────

  if (!isSetup) {
    return (
      <SafeAreaView style={s.setupContainer}>
        <StatusBar style="light" />
        <View style={s.setupCard}>
          <Text style={s.setupMark}>⬡</Text>
          <Text style={s.setupTitle}>claudulhu</Text>
          <Text style={s.setupDesc}>Enter the address of your claudulhu server.</Text>
          <TextInput
            style={s.setupInput}
            value={urlInput}
            onChangeText={setUrlInput}
            placeholder="192.168.1.x:8000"
            placeholderTextColor={C.textMuted}
            autoCapitalize="none"
            autoCorrect={false}
            keyboardType="url"
            returnKeyType="done"
            onSubmitEditing={saveUrl}
          />
          <TouchableOpacity
            style={[s.setupBtn, !urlInput.trim() && s.btnDisabled]}
            onPress={saveUrl}
            disabled={!urlInput.trim()}
          >
            <Text style={s.setupBtnText}>Connect</Text>
          </TouchableOpacity>
        </View>
      </SafeAreaView>
    )
  }

  // ── Main UI ────────────────────────────────────────────────────────────────

  const activeTabData = tabs.find(t => t.id === activeTab)

  return (
    <SafeAreaView style={s.container}>
      <StatusBar style="light" />

      {/* Header */}
      <View style={s.header}>
        <View style={s.headerLeft}>
          <Text style={s.headerMark}>⬡</Text>
          <Text style={s.headerTitle}>claudulhu</Text>
        </View>
        <View style={s.headerRight}>
          <TouchableOpacity style={s.branchesBtn} onPress={() => setShowBranches(true)}>
            <Text style={s.branchesBtnText}>⎇ {activeWorktrees}</Text>
          </TouchableOpacity>
          <TouchableOpacity
            style={s.settingsBtn}
            onPress={() => { setIsSetup(false); setTabs([]); setBranches([]) }}
          >
            <Text style={s.settingsBtnText}>⚙</Text>
          </TouchableOpacity>
        </View>
      </View>

      {/* Tab bar */}
      <ScrollView horizontal style={s.tabBar} contentContainerStyle={s.tabBarContent} showsHorizontalScrollIndicator={false}>
        {tabs.map(tab => (
          <TouchableOpacity
            key={tab.id}
            style={[s.tab, activeTab === tab.id && s.tabActive]}
            onPress={() => setActiveTab(tab.id)}
            activeOpacity={0.7}
          >
            <View style={[s.tabDot, { backgroundColor: statusColor(tabStatuses[tab.id] ?? 'connecting') }]} />
            <Text style={[s.tabLabel, activeTab === tab.id && s.tabLabelActive]} numberOfLines={1}>{tab.label}</Text>
            {tab.id !== 'main' && (
              <TouchableOpacity
                style={s.tabClose}
                onPress={() => closeTab(tab.id)}
                hitSlop={{ top: 8, bottom: 8, left: 4, right: 4 }}
              >
                <Text style={s.tabCloseText}>×</Text>
              </TouchableOpacity>
            )}
          </TouchableOpacity>
        ))}
      </ScrollView>

      {/* Chat panes (render all, show active) */}
      <KeyboardAvoidingView
        style={s.chatArea}
        behavior={Platform.OS === 'ios' ? 'padding' : 'height'}
        keyboardVerticalOffset={Platform.OS === 'ios' ? 0 : 0}
      >
        {tabs.map(tab => (
          <View key={tab.id} style={[s.chatPaneWrapper, tab.id !== activeTab && s.hidden]}>
            <ChatPane
              wsUrl={tab.wsUrl}
              active={tab.id === activeTab}
              canSpawnWorker={tab.id === 'main'}
              onStatusChange={handleStatusChange(tab.id)}
              onWorkerCreated={handleWorkerCreated}
              initialMessage={tab.initialMessage}
            />
          </View>
        ))}
      </KeyboardAvoidingView>

      {/* Branches modal */}
      <Modal visible={showBranches} animationType="slide" transparent onRequestClose={() => setShowBranches(false)}>
        <Pressable style={s.modalOverlay} onPress={() => setShowBranches(false)}>
          <Pressable style={s.modalSheet} onPress={e => e.stopPropagation()}>
            <View style={s.modalHandle} />
            <View style={s.modalHeader}>
              <Text style={s.modalTitle}>worktrees</Text>
              <Text style={s.modalCount}>{activeWorktrees}/{branches.length}</Text>
            </View>
            <FlatList
              data={branches}
              keyExtractor={b => b.name}
              style={s.branchesList}
              ListEmptyComponent={<Text style={s.branchesEmpty}>no branches found</Text>}
              renderItem={({ item: b }) => {
                const isOpen = tabs.some(t => t.id === b.name)
                return (
                  <TouchableOpacity
                    style={[s.branchItem, b.worktree && s.branchItemClickable]}
                    onPress={() => {
                      if (!b.worktree) return
                      openTab(b.name, b.worktree)
                      setShowBranches(false)
                    }}
                    activeOpacity={b.worktree ? 0.7 : 1}
                  >
                    <View style={[s.branchDot, b.worktree ? s.branchDotActive : s.branchDotInactive]} />
                    <View style={s.branchInfo}>
                      <Text style={s.branchName}>{b.name}</Text>
                      <Text style={s.branchCommit}>{b.commit}</Text>
                      {b.worktree && <Text style={s.branchWorktree} numberOfLines={1}>{b.worktree}</Text>}
                    </View>
                    {b.worktree && (
                      <Text style={s.branchHint}>{isOpen ? 'open' : 'chat'}</Text>
                    )}
                  </TouchableOpacity>
                )
              }}
            />
          </Pressable>
        </Pressable>
      </Modal>
    </SafeAreaView>
  )
}

// ── Styles ────────────────────────────────────────────────────────────────────

const s = StyleSheet.create({
  // Layout
  container:          { flex: 1, backgroundColor: C.bg },
  chatArea:           { flex: 1 },
  chatPaneWrapper:    { flex: 1 },
  hidden:             { display: 'none' },

  // Setup
  setupContainer:     { flex: 1, backgroundColor: C.bg, alignItems: 'center', justifyContent: 'center' },
  setupCard:          { width: '85%', alignItems: 'center', gap: 16 },
  setupMark:          { fontSize: 40, color: C.accent },
  setupTitle:         { fontSize: 24, fontWeight: '700', color: C.textPrimary, letterSpacing: 2 },
  setupDesc:          { fontSize: 14, color: C.textSecondary, textAlign: 'center' },
  setupInput:         {
    width: '100%', backgroundColor: C.inputBg, borderWidth: 1, borderColor: C.inputBorder,
    borderRadius: 8, paddingHorizontal: 14, paddingVertical: 12, color: C.textPrimary,
    fontSize: 16, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace',
  },
  setupBtn:           { backgroundColor: C.accent, borderRadius: 8, paddingHorizontal: 32, paddingVertical: 12, width: '100%', alignItems: 'center' },
  setupBtnText:       { color: '#fff', fontWeight: '600', fontSize: 16 },

  // Header
  header:             { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: 16, paddingVertical: 10, borderBottomWidth: 1, borderBottomColor: C.border },
  headerLeft:         { flexDirection: 'row', alignItems: 'center', gap: 8 },
  headerRight:        { flexDirection: 'row', alignItems: 'center', gap: 8 },
  headerMark:         { fontSize: 18, color: C.accent },
  headerTitle:        { fontSize: 16, fontWeight: '700', color: C.textPrimary, letterSpacing: 1 },
  branchesBtn:        { backgroundColor: C.surface, borderWidth: 1, borderColor: C.border, borderRadius: 6, paddingHorizontal: 10, paddingVertical: 5 },
  branchesBtnText:    { color: C.textSecondary, fontSize: 13 },
  settingsBtn:        { backgroundColor: C.surface, borderWidth: 1, borderColor: C.border, borderRadius: 6, paddingHorizontal: 10, paddingVertical: 5 },
  settingsBtnText:    { color: C.textSecondary, fontSize: 14 },

  // Tabs
  tabBar:             { flexGrow: 0, backgroundColor: C.surface, borderBottomWidth: 1, borderBottomColor: C.border, maxHeight: 40 },
  tabBarContent:      { paddingHorizontal: 8, alignItems: 'center', gap: 4 },
  tab:                { flexDirection: 'row', alignItems: 'center', paddingHorizontal: 10, paddingVertical: 8, gap: 5, borderRadius: 4, marginVertical: 4 },
  tabActive:          { backgroundColor: C.bg },
  tabDot:             { width: 6, height: 6, borderRadius: 3 },
  tabLabel:           { color: C.textMuted, fontSize: 13, maxWidth: 100 },
  tabLabelActive:     { color: C.textPrimary },
  tabClose:           { marginLeft: 2 },
  tabCloseText:       { color: C.textMuted, fontSize: 15, lineHeight: 16 },

  // Chat pane
  chatPane:           { flex: 1, backgroundColor: C.bg },
  messageScroll:      { flex: 1 },
  messageScrollContent: { paddingVertical: 16 },
  emptyState:         { textAlign: 'center', color: C.textMuted, fontSize: 14, marginTop: 60 },

  // Messages
  messagePadding:     { paddingHorizontal: 14, marginBottom: 14 },
  messageRight:       { alignItems: 'flex-end' },
  messageLabel:       { fontSize: 11, color: C.textMuted, marginBottom: 4, marginLeft: 2, fontWeight: '500', letterSpacing: 0.5 },
  messageLabelUser:   { marginLeft: 0, marginRight: 2 },

  // Bubbles
  bubble:             { borderRadius: 12, padding: 12, maxWidth: '92%' },
  userBubble:         { backgroundColor: C.userBubble, borderWidth: 1, borderColor: C.userBorder },
  asstBubble:         { backgroundColor: C.asstBubble, borderWidth: 1, borderColor: C.asstBorder },
  infoBubble:         { backgroundColor: C.infoBubble, borderWidth: 1, borderColor: C.border },
  cursor:             { color: C.accent, fontSize: 14 },

  // Blocks
  textBlock:          { color: C.textPrimary, fontSize: 15, lineHeight: 22 },
  errorBlock:         { color: C.red, fontSize: 14 },
  interruptedBlock:   { color: C.textMuted, fontSize: 14, fontStyle: 'italic' },
  systemBlock:        { color: C.textSecondary, fontSize: 13 },
  thinkingDots:       { color: C.textMuted, fontSize: 18, letterSpacing: 4 },

  resultFooter:       { flexDirection: 'row', justifyContent: 'space-between', marginTop: 4, paddingTop: 6, borderTopWidth: 1, borderTopColor: C.border },
  resultFooterText:   { color: C.textMuted, fontSize: 12 },

  questionBlock:      { flexDirection: 'row', alignItems: 'flex-start', gap: 8, marginTop: 2 },
  questionIcon:       { color: C.yellow, fontSize: 16, fontWeight: '700' },
  questionText:       { color: C.textPrimary, fontSize: 15, flex: 1, lineHeight: 22 },

  workerCreatedBlock: { flexDirection: 'row', alignItems: 'flex-start', gap: 8 },
  workerCreatedIcon:  { color: C.accent, fontSize: 16 },
  workerCreatedBranch:{ color: C.accent, fontSize: 14, fontWeight: '600' },
  workerCreatedPath:  { color: C.textMuted, fontSize: 12, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace' },

  // Tool blocks
  toolBlock:          { backgroundColor: C.toolBg, borderWidth: 1, borderColor: C.toolBorder, borderRadius: 8, overflow: 'hidden', marginTop: 4 },
  toolHeader:         { flexDirection: 'row', alignItems: 'center', padding: 8, gap: 6 },
  toolIcon:           { color: C.yellow, fontSize: 13 },
  toolName:           { color: C.textSecondary, fontSize: 13, flex: 1, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace' },
  toolToggle:         { color: C.textMuted, fontSize: 11 },
  toolBody:           { maxHeight: 200, padding: 8 },
  toolBodyText:       { color: C.textSecondary, fontSize: 12, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace', lineHeight: 18 },

  toolResultBlock:    { backgroundColor: C.toolBg, borderWidth: 1, borderColor: C.toolBorder, borderRadius: 8, overflow: 'hidden', marginTop: 4 },
  toolResultHeader:   { flexDirection: 'row', alignItems: 'center', padding: 8, gap: 6 },
  resultIcon:         { color: C.green, fontSize: 13 },
  resultPreview:      { color: C.textMuted, fontSize: 12, flex: 1, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace' },

  // Input
  inputArea:          { flexDirection: 'row', alignItems: 'flex-end', paddingHorizontal: 12, paddingVertical: 10, borderTopWidth: 1, borderTopColor: C.border, gap: 8, backgroundColor: C.surface },
  textInput:          {
    flex: 1, backgroundColor: C.inputBg, borderWidth: 1, borderColor: C.inputBorder,
    borderRadius: 10, paddingHorizontal: 12, paddingVertical: 10, color: C.textPrimary,
    fontSize: 15, lineHeight: 22, maxHeight: 120,
  },
  inputButtons:       { justifyContent: 'flex-end', paddingBottom: 2 },
  btnSend:            { backgroundColor: C.accent, borderRadius: 8, width: 36, height: 36, alignItems: 'center', justifyContent: 'center' },
  btnSendText:        { color: '#fff', fontSize: 15 },
  btnInterrupt:       { backgroundColor: C.red, borderRadius: 8, paddingHorizontal: 10, height: 36, alignItems: 'center', justifyContent: 'center' },
  btnInterruptText:   { color: '#fff', fontSize: 13, fontWeight: '600' },
  btnDisabled:        { opacity: 0.3 },
  btnDisabledText:    { color: C.textMuted },

  // Branches modal
  modalOverlay:       { flex: 1, justifyContent: 'flex-end', backgroundColor: 'rgba(0,0,0,0.5)' },
  modalSheet:         { backgroundColor: C.surface, borderTopLeftRadius: 16, borderTopRightRadius: 16, paddingBottom: 32, maxHeight: '70%' },
  modalHandle:        { width: 36, height: 4, backgroundColor: C.borderLight, borderRadius: 2, alignSelf: 'center', marginTop: 10, marginBottom: 4 },
  modalHeader:        { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: 20, paddingVertical: 12, borderBottomWidth: 1, borderBottomColor: C.border },
  modalTitle:         { color: C.textPrimary, fontSize: 16, fontWeight: '600' },
  modalCount:         { color: C.textMuted, fontSize: 14 },

  branchesList:       { paddingHorizontal: 4 },
  branchesEmpty:      { color: C.textMuted, fontSize: 14, textAlign: 'center', padding: 24 },
  branchItem:         { flexDirection: 'row', alignItems: 'center', paddingHorizontal: 16, paddingVertical: 12, gap: 10, borderBottomWidth: 1, borderBottomColor: C.border },
  branchItemClickable:{ },
  branchDot:          { width: 8, height: 8, borderRadius: 4 },
  branchDotActive:    { backgroundColor: C.green },
  branchDotInactive:  { backgroundColor: C.textMuted },
  branchInfo:         { flex: 1 },
  branchName:         { color: C.textPrimary, fontSize: 14, fontWeight: '500' },
  branchCommit:       { color: C.textMuted, fontSize: 12, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace' },
  branchWorktree:     { color: C.textMuted, fontSize: 11, marginTop: 2, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace' },
  branchHint:         { color: C.accent, fontSize: 12 },
})
