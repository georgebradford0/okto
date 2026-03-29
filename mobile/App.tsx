import AsyncStorage from '@react-native-async-storage/async-storage'
import { useCallback, useEffect, useRef, useState } from 'react'
import {
  Animated,
  ActivityIndicator,
  AppState,
  FlatList,
  Image,
  KeyboardAvoidingView,
  Modal,
  PermissionsAndroid,
  Platform,
  Pressable,
  ScrollView,
  StatusBar,
  StyleSheet,
  Text,
  TextInput,
  TouchableOpacity,
  View,
} from 'react-native'
import { SafeAreaProvider, SafeAreaView } from 'react-native-safe-area-context'
import { Camera, useCameraDevice, useCodeScanner } from 'react-native-vision-camera'
import NoiseConnection from './src/NativeNoiseConnection'

// ── Types ──────────────────────────────────────────────────────────────────────

interface NoiseConnectionInfo {
  v:      number
  host:   string
  port:   number
  pk:     string    // base32-encoded 32-byte Curve25519 server static public key
  label?: string    // repo name, populated after first successful connect
}

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
  initialMessage?: string
}

type ConnStatus = 'connecting' | 'ready' | 'resumed' | 'error' | 'disconnected'

// ── Helpers ────────────────────────────────────────────────────────────────────

let _id = 0
const uid = () => `m${++_id}`

function parseQrData(raw: string): NoiseConnectionInfo | null {
  // Format v2: "2:host:port:pk_base32"
  // pk_base32 = base32(32-byte Curve25519 server static public key), 52 chars
  // All chars uppercase+digits+colon → QR alphanumeric mode → compact QR.
  const parts = raw.split(':')
  if (parts[0] === '2' && parts.length === 4) {
    const [, host, portStr, pk] = parts
    const port = parseInt(portStr, 10)
    if (!host || isNaN(port) || !pk) return null
    return { v: 2, host, port, pk }
  }
  return null
}

// ── Colours ────────────────────────────────────────────────────────────────────

const C = {
  bg:            '#ffffff',
  surface:       '#f2f2f7',
  border:        '#d1d1d6',
  borderLight:   '#e5e5ea',
  accent:        '#2563eb',
  green:         '#22863a',
  yellow:        '#b45309',
  red:           '#dc2626',
  textPrimary:   '#1c1c1e',
  textSecondary: '#6b6b6b',
  textMuted:     '#aeaeb2',
  userBubble:    '#dbeafe',
  userBorder:    '#93c5fd',
  asstBubble:    '#f2f2f7',
  asstBorder:    '#d1d1d6',
  infoBubble:    '#f9f9fb',
  toolBg:        '#f2f2f7',
  inputBg:       '#ffffff',
  inputBorder:   '#d1d1d6',
}

// ── ToolUseBlock ──────────────────────────────────────────────────────────────

const PRIMARY_KEYS = ['command', 'path', 'query', 'input', 'content', 'url', 'pattern', 'prompt']

function primaryArg(input: Record<string, unknown>): string {
  for (const key of PRIMARY_KEYS) {
    if (typeof input[key] === 'string') return input[key] as string
  }
  const first = Object.values(input)[0]
  return first !== undefined ? String(first) : ''
}

function ToolUseBlock({ tool, input }: { tool: string; input: Record<string, unknown> }) {
  const arg = primaryArg(input)
  return (
    <Text style={s.toolLine}>{tool}("{arg}")</Text>
  )
}

// ── ToolResultBlock ────────────────────────────────────────────────────────────

function ToolResultBlock({ content: _ }: { content: unknown }) {
  return null
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
          <Text style={s.resultMeta}>✓ {block.turns} turn{block.turns !== 1 ? 's' : ''}</Text>
          <Text style={s.resultMeta}>${block.cost_usd.toFixed(4)}</Text>
        </View>
      )
    case 'error':
      return <Text style={s.errorText}>✗ {block.message}</Text>
    case 'interrupted':
      return <Text style={s.mutedText}>— interrupted</Text>
    case 'question':
      return (
        <View style={s.questionRow}>
          <Text style={s.questionMark}>?</Text>
          <Text style={s.questionText}>{block.question}</Text>
        </View>
      )
    case 'system':
      return <Text style={s.systemText}>{block.text}</Text>
    case 'worker_created':
      return (
        <View style={s.workerRow}>
          <Text style={s.workerIcon}>⎇</Text>
          <View style={s.workerInfo}>
            <Text style={s.workerBranch}>{block.branch}</Text>
            <Text style={s.workerPath} numberOfLines={1}>{block.worktree_path}</Text>
          </View>
        </View>
      )
    case 'worker_error':
      return <Text style={s.errorText}>✗ {block.message}</Text>
  }
}

// ── PendingEllipsis ───────────────────────────────────────────────────────────

function PendingEllipsis() {
  const dots = [useRef(new Animated.Value(0)).current, useRef(new Animated.Value(0)).current, useRef(new Animated.Value(0)).current]

  useEffect(() => {
    const anims = dots.map((dot, i) =>
      Animated.loop(
        Animated.sequence([
          Animated.delay(i * 150),
          Animated.timing(dot, { toValue: 1, duration: 300, useNativeDriver: true }),
          Animated.timing(dot, { toValue: 0, duration: 300, useNativeDriver: true }),
          Animated.delay((dots.length - i - 1) * 150),
        ])
      )
    )
    anims.forEach(a => a.start())
    return () => anims.forEach(a => a.stop())
  }, [])

  return (
    <View style={[s.messageWrap]}>
      <Text style={s.messageLabel}>claude</Text>
      <View style={{ flexDirection: 'row', alignItems: 'center', gap: 4 }}>
        {dots.map((dot, i) => (
          <Animated.Text key={i} style={[s.cursor, { opacity: dot }]}>●</Animated.Text>
        ))}
      </View>
    </View>
  )
}

// ── MessageBubble ─────────────────────────────────────────────────────────────

function MessageBubble({ message }: { message: ChatMessage }) {
  const isUser = message.role === 'user'
  const isInfo = message.role === 'info'
  return (
    <View style={[s.messageWrap, isUser && s.messageWrapRight]}>
      {!isInfo && (
        <Text style={[s.messageLabel, isUser && s.messageLabelRight]}>
          {isUser ? 'you' : 'claude'}
        </Text>
      )}
      {message.blocks.map((block, i) => <BlockRenderer key={i} block={block} />)}
      {message.streaming && <Text style={s.cursor}>▋</Text>}
    </View>
  )
}

// ── CreatureAnim ──────────────────────────────────────────────────────────────

function CreatureAnim() {
  const slideX = useRef(new Animated.Value(-300)).current

  useEffect(() => {
    Animated.timing(slideX, { toValue: 0, duration: 700, useNativeDriver: true }).start()
  }, [])

  return (
    <Animated.Image
      source={require('./assets/creature.png')}
      style={[s.creatureImg, { transform: [{ translateX: slideX }] }]}
    />
  )
}

// ── QrScanner ─────────────────────────────────────────────────────────────────

function QrScanner({ onScanned, onCancel }: { onScanned: (data: string) => void; onCancel: () => void }) {
  const device = useCameraDevice('back')
  const scannedRef = useRef(false)

  const codeScanner = useCodeScanner({
    codeTypes: ['qr'],
    onCodeScanned: (codes) => {
      console.log('[scanner] codes:', JSON.stringify(codes))
      if (scannedRef.current) { return }
      const value = codes[0]?.value
      console.log('[scanner] value:', value)
      if (value) {
        scannedRef.current = true
        onScanned(value)
      }
    },
  })

  if (!device) {
    return (
      <View style={s.scannerFull}>
        <Text style={s.scannerError}>Camera not available</Text>
        <TouchableOpacity style={s.scannerCancel} onPress={onCancel}>
          <Text style={s.scannerCancelText}>Cancel</Text>
        </TouchableOpacity>
      </View>
    )
  }

  return (
    <View style={s.scannerFull}>
      <Camera
        device={device}
        isActive={true}
        codeScanner={codeScanner}
        style={StyleSheet.absoluteFill}
      />
      {/* Dark overlay with cut-out reticle */}
      <View style={s.scannerOverlay}>
        <View style={s.scannerTopBar}>
          <Text style={s.scannerTitle}>Scan QR code</Text>
          <Text style={s.scannerSubtitle}>Point at the QR code printed in the container terminal</Text>
        </View>
        <View style={s.scannerReticle}>
          <View style={[s.scannerCorner, s.cornerTL]} />
          <View style={[s.scannerCorner, s.cornerTR]} />
          <View style={[s.scannerCorner, s.cornerBL]} />
          <View style={[s.scannerCorner, s.cornerBR]} />
        </View>
        <TouchableOpacity style={s.scannerCancel} onPress={onCancel}>
          <Text style={s.scannerCancelText}>Cancel</Text>
        </TouchableOpacity>
      </View>
    </View>
  )
}

// ── ChatPane ──────────────────────────────────────────────────────────────────

interface ChatPaneProps {
  wsUrl:           string
  tunnelPort:      number
  branches:        Branch[]
  canSpawnWorker:  boolean
  onStatusChange:  (s: ConnStatus) => void
  onWorkerCreated: (branch: string, worktreePath: string, task: string) => void
  initialMessage?: string
}

function parseAtCompletion(text: string): { atPos: number; dirPart: string; filePart: string } | null {
  const atIdx = text.lastIndexOf('@')
  if (atIdx === -1) return null
  const fragment = text.slice(atIdx + 1)
  if (fragment.includes(' ')) return null
  const slash = fragment.lastIndexOf('/')
  const dirPart  = slash >= 0 ? fragment.slice(0, slash + 1) : ''
  const filePart = slash >= 0 ? fragment.slice(slash + 1)    : fragment
  return { atPos: atIdx, dirPart, filePart }
}

function ChatPane({ wsUrl, tunnelPort, branches, canSpawnWorker, onStatusChange, onWorkerCreated, initialMessage }: ChatPaneProps) {
  const [messages,        setMessages]        = useState<ChatMessage[]>([])
  const [status,          setStatus]          = useState<ConnStatus>('connecting')
  const [isStreaming,     setIsStreaming]      = useState(false)
  const [isPending,       setIsPending]       = useState(false)
  const [pendingQuestion, setPendingQuestion] = useState(false)
  const [input,           setInput]           = useState('')
  const [completions,     setCompletions]     = useState<string[]>([])
  const [compQuery,       setCompQuery]       = useState<{ atPos: number; dirPart: string; filePart: string } | null>(null)

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
      if (!last?.streaming) { return prev }
      if (block.kind === 'text') {
        const tail = last.blocks[last.blocks.length - 1]
        if (tail?.kind === 'text') {
          return prev.map((m, i) => i < prev.length - 1 ? m : {
            ...m, blocks: [...m.blocks.slice(0, -1), { kind: 'text' as const, text: tail.text + block.text }],
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
    // Clear streaming on ALL messages, not just the last — belt-and-suspenders in
    // case an earlier bubble was left streaming by a skipped completeResponse.
    setMessages(prev => prev.map(m => m.streaming ? { ...m, streaming: false } : m))
  }, [])

  const handleFrame = useCallback((frame: ServerFrame) => {
    switch (frame.type) {
      case 'ready':
        updateStatus(frame.resumed ? 'resumed' : 'ready')
        break
      case 'text':
        ensureAssistantMsg(); appendBlock({ kind: 'text', text: frame.text })
        break
      case 'tool_use':
        ensureAssistantMsg(); appendBlock({ kind: 'tool_use', tool: frame.tool, input: frame.input })
        break
      case 'tool_result':
        if (!inResponseRef.current) {
          // Spurious tool_result (e.g. duplicate, resumed session mid-tool-call).
          // appendBlock would silently drop it anyway, and setting isPending=true
          // here with nothing to clear it is what causes the hanging dots.
          break
        }
        appendBlock({ kind: 'tool_result', content: frame.content })
        inResponseRef.current = false
        setIsStreaming(false)
        setIsPending(true)
        setMessages(prev => prev.map((m, i) => i < prev.length - 1 ? m : { ...m, streaming: false }))
        break
      case 'result':
        // ensureAssistantMsg in case result arrives while pending (tool_result → result
        // with no intervening text/tool_use frame), so appendBlock has a streaming msg.
        ensureAssistantMsg(); appendBlock({ kind: 'result', cost_usd: frame.cost_usd, turns: frame.turns })
        completeResponse()
        break
      case 'error':
        ensureAssistantMsg(); appendBlock({ kind: 'error', message: frame.message }); completeResponse()
        break
      case 'interrupted':
        ensureAssistantMsg(); appendBlock({ kind: 'interrupted' }); completeResponse()
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
      if (cancelled) { return }
      console.log('[ws] connecting to', wsUrl)
      updateStatus('connecting')
      const ws = new WebSocket(wsUrl)
      wsRef.current = ws

      ws.onopen = () => {
        console.log('[ws] connected to', wsUrl)
        updateStatus('ready')
      }

      ws.onmessage = ({ data }) => {
        let frame: ServerFrame
        try { frame = JSON.parse(data as string) } catch {
          console.warn('[ws] failed to parse frame:', data)
          return
        }
        console.log('[ws] frame:', frame.type)
        handleFrame(frame)
      }

      ws.onclose = (e) => {
        console.log('[ws] closed', wsUrl, 'code:', e.code, 'reason:', e.reason)
        if (!cancelled) {
          // Clear ALL in-flight state regardless of inResponseRef — isPending can be
          // true even when inResponseRef=false (between tool_result and the next frame).
          inResponseRef.current = false
          setIsStreaming(false)
          setIsPending(false)
          setPendingQuestion(false)
          setMessages(prev => prev.map((m, i) => i < prev.length - 1 ? m : { ...m, streaming: false }))
          updateStatus('disconnected')
          setTimeout(connect, 3000)
        }
      }

      ws.onerror = (e) => {
        console.error('[ws] error on', wsUrl, e)
        inResponseRef.current = false
        setIsStreaming(false)
        setIsPending(false)
        setPendingQuestion(false)
        setMessages(prev => prev.map(m => m.streaming ? { ...m, streaming: false } : m))
        updateStatus('error')
      }
    }

    connect()
    return () => {
      cancelled = true
      wsRef.current?.close()
    }
  }, [wsUrl, handleFrame, updateStatus])

  // Force-reconnect when returning to foreground — iOS silently kills the socket
  // while backgrounded, leaving readyState=OPEN but the connection dead.
  useEffect(() => {
    const sub = AppState.addEventListener('change', nextState => {
      if (nextState === 'active') {
        wsRef.current?.close()
      }
    })
    return () => sub.remove()
  }, [])

  // Send initial message once ready
  useEffect(() => {
    if (!initialMessage || initialMessageSent.current) { return }
    if (status !== 'ready' && status !== 'resumed') { return }
    initialMessageSent.current = true
    setMessages(prev => [...prev, {
      id: uid(), role: 'user', streaming: false,
      blocks: [{ kind: 'text', text: initialMessage }],
    }])
    wsRef.current?.send(JSON.stringify({ type: 'message', text: initialMessage }))
  }, [status, initialMessage])

  const sendMessage = useCallback(() => {
    const text = input.trim()
    if (!text) { return }
    if (isStreaming && !pendingQuestion) { return }
    const ws = wsRef.current
    if (!ws || ws.readyState !== WebSocket.OPEN) { return }

    if (pendingQuestion) {
      setMessages(prev => [...prev, { id: uid(), role: 'user', streaming: false, blocks: [{ kind: 'text', text }] }])
      ws.send(JSON.stringify({ type: 'answer', answer: text }))
      setPendingQuestion(false)
      setIsPending(true)
    } else {
      setMessages(prev => [...prev, { id: uid(), role: 'user', streaming: false, blocks: [{ kind: 'text', text }] }])
      ws.send(JSON.stringify({ type: 'message', text }))
      setIsPending(true)
    }
    setInput(''); setCompletions([]); setCompQuery(null)
  }, [input, isStreaming, pendingQuestion])

  const spawnWorker = useCallback(() => {
    const text = input.trim()
    if (!text || isStreaming) { return }
    const ws = wsRef.current
    if (!ws || ws.readyState !== WebSocket.OPEN) { return }
    ws.send(JSON.stringify({ type: 'spawn_worker', task: text }))
    setMessages(prev => [...prev, { id: uid(), role: 'user', streaming: false, blocks: [{ kind: 'text', text }] }])
    setInput(''); setCompletions([]); setCompQuery(null)
    setIsPending(true)
    inResponseRef.current = true
    setIsStreaming(true)
  }, [input, isStreaming])

  const updateCompletions = useCallback((query: { atPos: number; dirPart: string; filePart: string }) => {
    setCompQuery(query)
    const worktreeItems = query.dirPart === ''
      ? branches
          .filter(b => b.worktree && b.name.toLowerCase().startsWith(query.filePart.toLowerCase()))
          .map(b => `⎇ ${b.name}`)
      : []
    const params = new URLSearchParams({ dir_part: query.dirPart, file_part: query.filePart })
    fetch(`http://127.0.0.1:${tunnelPort}/completions?${params}`)
      .then(r => r.ok ? (r.json() as Promise<string[]>) : Promise.resolve([]))
      .then(items => setCompletions([...worktreeItems, ...items]))
      .catch(() => setCompletions(worktreeItems))
  }, [branches, tunnelPort])

  const handleInputChange = useCallback((text: string) => {
    setInput(text)
    const query = parseAtCompletion(text)
    if (!query) { setCompletions([]); setCompQuery(null); return }
    updateCompletions(query)
  }, [updateCompletions])

  const acceptCompletion = useCallback((completion: string) => {
    if (!compQuery) return
    const inserted = completion.startsWith('⎇ ') ? completion.slice(2) : completion
    const suffix = inserted.endsWith('/') ? '' : ' '
    const next = input.slice(0, compQuery.atPos + 1) + inserted + suffix
    setInput(next)
    const newQuery = inserted.endsWith('/') ? parseAtCompletion(next) : null
    if (newQuery) {
      updateCompletions(newQuery)
    } else {
      setCompletions([])
      setCompQuery(null)
    }
  }, [compQuery, input, updateCompletions])

  const canSend = !!input.trim() && (pendingQuestion || (!isStreaming && (status === 'ready' || status === 'resumed')))

  return (
    <View style={s.pane}>
      <ScrollView
        ref={scrollRef}
        style={s.messageList}
        contentContainerStyle={s.messageListContent}
        onContentSizeChange={() => scrollRef.current?.scrollToEnd({ animated: true })}
      >
        {messages.length === 0 && (
          <Text style={s.emptyState}>
            {status === 'connecting' || status === 'disconnected' ? 'connecting to server…' : 'send a message to begin'}
          </Text>
        )}
        {messages.map(m => <MessageBubble key={m.id} message={m} />)}
        {isPending && <PendingEllipsis />}
      </ScrollView>

      {completions.length > 0 && (
        <ScrollView style={s.completionList} keyboardShouldPersistTaps="always">
          {completions.map((c, i) => (
            <TouchableOpacity key={`${c}${i}`} style={s.completionItem} onPress={() => acceptCompletion(c)}>
              <Text style={s.completionText} numberOfLines={1}>{c}</Text>
            </TouchableOpacity>
          ))}
        </ScrollView>
      )}
      <View style={s.inputRow}>
        <TextInput
          style={s.input}
          value={input}
          onChangeText={handleInputChange}
          placeholder={pendingQuestion ? 'answer…' : 'message…'}
          placeholderTextColor={C.textMuted}
          multiline
          returnKeyType="default"
          editable={pendingQuestion || status === 'ready' || status === 'resumed'}
        />
        {canSpawnWorker && !!input.trim() && !isStreaming && (status === 'ready' || status === 'resumed') && !pendingQuestion && (
          <TouchableOpacity style={[s.btnSend, { backgroundColor: C.yellow }]} onPress={spawnWorker}>
            <Text style={s.btnSendText}>⎇</Text>
          </TouchableOpacity>
        )}
        {isStreaming ? (
          <TouchableOpacity style={s.btnStop} onPress={() => wsRef.current?.send(JSON.stringify({ type: 'interrupt' }))}>
            <Text style={s.btnStopText}>■</Text>
          </TouchableOpacity>
        ) : (
          <TouchableOpacity style={[s.btnSend, !canSend && s.btnDisabled]} onPress={sendMessage} disabled={!canSend}>
            <Text style={s.btnSendText}>↑</Text>
          </TouchableOpacity>
        )}
      </View>
    </View>
  )
}

// ── ConnRow ───────────────────────────────────────────────────────────────────

function ConnRow({ conn, isConnecting, error, onSelect, onDelete }: {
  conn:         NoiseConnectionInfo
  isConnecting: boolean
  error:        string | null
  onSelect:     () => void
  onDelete:     () => void
}) {
  return (
    <TouchableOpacity style={s.connRow} onPress={onSelect} activeOpacity={0.7}>
      <View style={s.connInfo}>
        <Text style={s.connLabel}>{conn.label ?? `${conn.host}:${conn.port}`}</Text>
        <Text style={s.connHost}>{conn.host}:{conn.port}</Text>
        {error && <Text style={s.connError} numberOfLines={2}>{error}</Text>}
      </View>
      {isConnecting
        ? <ActivityIndicator size="small" color={C.accent} />
        : (
          <TouchableOpacity onPress={onDelete} hitSlop={{ top: 10, bottom: 10, left: 10, right: 10 }}>
            <Text style={s.connDelete}>✕</Text>
          </TouchableOpacity>
        )
      }
    </TouchableOpacity>
  )
}

// ── Root App ──────────────────────────────────────────────────────────────────

function AppInner() {
  const [conn,         setConn]         = useState<NoiseConnectionInfo | null>(null)
  const [tunnelPort,   setTunnelPort]   = useState<number | null>(null)
  const [tunnelError,  setTunnelError]  = useState<string | null>(null)
  const [scanning,     setScanning]     = useState(false)
  const [tabs,         setTabs]         = useState<Tab[]>([])
  const [activeTab,    setActiveTab]    = useState('main')
  const [tabStatuses,  setTabStatuses]  = useState<Record<string, ConnStatus>>({ main: 'connecting' })
  const [branches,     setBranches]     = useState<Branch[]>([])
  const [showBranches, setShowBranches] = useState(false)
  const [repoName,     setRepoName]     = useState<string | null>(null)
  const [savedConns,   setSavedConns]   = useState<NoiseConnectionInfo[]>([])

  // ── Load stored connections on mount ──────────────────────────────────────
  useEffect(() => {
    AsyncStorage.getItem('noiseConnections').then(json => {
      let conns: NoiseConnectionInfo[] = []
      if (json) { try { conns = JSON.parse(json) } catch {} }
      setSavedConns(conns)
      if (conns.length === 1) { setConn(conns[0]) }
    })
  }, [])

  // ── Establish Noise tunnel whenever conn changes ──────────────────────────
  useEffect(() => {
    setTunnelPort(null)
    setTunnelError(null)
    setRepoName(null)
    setBranches([])
    setTabs([])
    if (!conn) { return }

    NoiseConnection.connect(conn.host, conn.port, conn.pk)
      .then(port => { setTunnelPort(port) })
      .catch(e  => { setTunnelError(e?.message ?? String(e)) })

    return () => { NoiseConnection.disconnect() }
  }, [conn])

  // ── Init tabs when tunnel is ready ─────────────────────────────────────────
  useEffect(() => {
    if (!tunnelPort) { return }
    setTabs([{ id: 'main', label: 'main', wsUrl: `ws://127.0.0.1:${tunnelPort}/chat` }])
    setActiveTab('main')
    setTabStatuses({ main: 'connecting' })
  }, [tunnelPort])

  // ── Fetch repo name ────────────────────────────────────────────────────────
  useEffect(() => {
    if (!tunnelPort || !conn) { return }
    fetch(`http://127.0.0.1:${tunnelPort}/config`)
      .then(r => r.ok ? r.json() : null)
      .then((d: { repo?: string | null; name?: string | null } | null) => {
        const name = d?.name ?? null
        setRepoName(name)
        if (name) {
          setSavedConns(prev => {
            const updated = prev.map(c =>
              c.host === conn.host && c.port === conn.port ? { ...c, label: name } : c
            )
            AsyncStorage.setItem('noiseConnections', JSON.stringify(updated))
            return updated
          })
        }
      })
      .catch(() => {})
  }, [tunnelPort, conn])

  // ── Poll branches ──────────────────────────────────────────────────────────
  useEffect(() => {
    if (!tunnelPort) { return }
    const poll = () => {
      fetch(`http://127.0.0.1:${tunnelPort}/branches`)
        .then(r => r.ok ? r.json() : null)
        .then((d: Branch[] | null) => d && setBranches(d))
        .catch(() => {})
    }
    poll()
    const t = setInterval(poll, 10_000)
    return () => clearInterval(t)
  }, [tunnelPort])

  // ── Auto-close tabs whose worktree was removed ─────────────────────────────
  useEffect(() => {
    setTabs(prev => {
      const toClose = prev.filter(t => {
        if (t.id === 'main') { return false }
        const b = branches.find(b => b.name === t.id)
        return !b || !b.worktree
      })
      if (toClose.length === 0) { return prev }
      const closeIds = new Set(toClose.map(t => t.id))
      setTabStatuses(s => { const n = { ...s }; closeIds.forEach(id => delete n[id]); return n })
      setActiveTab(cur => closeIds.has(cur) ? 'main' : cur)
      return prev.filter(t => !closeIds.has(t.id))
    })
  }, [branches])

  // ── QR scan handler ────────────────────────────────────────────────────────
  const handleQrScanned = useCallback((raw: string) => {
    setScanning(false)
    console.log('[QR] raw:', JSON.stringify(raw))
    const parsed = parseQrData(raw)
    console.log('[QR] parsed:', parsed)
    if (!parsed) {
      setTunnelError('Invalid QR code — not a claudulhu connection QR')
      return
    }
    setSavedConns(prev => {
      const updated = [
        ...prev.filter(c => !(c.host === parsed.host && c.port === parsed.port)),
        parsed,
      ]
      AsyncStorage.setItem('noiseConnections', JSON.stringify(updated))
      return updated
    })
    setConn(parsed)
  }, [])

  const requestCameraAndScan = useCallback(async () => {
    if (Platform.OS === 'android') {
      const granted = await PermissionsAndroid.request(PermissionsAndroid.PERMISSIONS.CAMERA)
      if (granted !== PermissionsAndroid.RESULTS.GRANTED) { return }
    }
    setScanning(true)
  }, [])

  const selectConn = useCallback((c: NoiseConnectionInfo) => {
    setConn(c)
  }, [])

  const deleteConn = useCallback((c: NoiseConnectionInfo) => {
    setSavedConns(prev => {
      const updated = prev.filter(x => !(x.host === c.host && x.port === c.port))
      AsyncStorage.setItem('noiseConnections', JSON.stringify(updated))
      return updated
    })
    if (conn?.host === c.host && conn?.port === c.port) {
      setConn(null)
    }
  }, [conn])

  const openTab = useCallback((branch: string, _worktreePath: string, initialMessage?: string) => {
    if (!tunnelPort) { return }
    setTabs(prev => {
      if (prev.find(t => t.id === branch)) { return prev }
      return [...prev, { id: branch, label: branch, wsUrl: `ws://127.0.0.1:${tunnelPort}/workers/${encodeURIComponent(branch)}`, initialMessage }]
    })
    setTabStatuses(prev => ({ ...prev, [branch]: prev[branch] ?? 'connecting' }))
    setActiveTab(branch)
  }, [tunnelPort])

  const closeTab = useCallback((id: string) => {
    if (id === 'main') { return }
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

  const statusColor = (st: ConnStatus) => {
    if (st === 'ready' || st === 'resumed') { return C.green }
    if (st === 'error') { return C.red }
    return C.yellow
  }

  const activeWorktrees = branches.filter(b => b.worktree).length
  const openTabIds = new Set(tabs.map(t => t.id))

  // ── QR scanner overlay ─────────────────────────────────────────────────────
  if (scanning) {
    return (
      <QrScanner
        onScanned={handleQrScanned}
        onCancel={() => setScanning(false)}
      />
    )
  }

  // ── Setup / connecting screen ──────────────────────────────────────────────
  if (!tunnelPort) {
    return (
      <SafeAreaView style={s.setupSafe} edges={['top', 'bottom']}>
        {savedConns.length === 0 ? (
          <View style={s.setupCenter}>
            <CreatureAnim />
            <Text style={s.setupTitle}>claudulhu</Text>
            <Text style={s.setupDesc}>Scan the QR code printed by the Docker container</Text>
            <TouchableOpacity style={s.setupBtn} onPress={requestCameraAndScan}>
              <Text style={s.setupBtnText}>Scan QR code</Text>
            </TouchableOpacity>
          </View>
        ) : (
          <View style={s.pickerWrap}>
            <View style={s.pickerHeader}>
              <CreatureAnim />
              <Text style={s.setupTitle}>claudulhu</Text>
            </View>
            <FlatList
              data={savedConns}
              keyExtractor={c => `${c.host}:${c.port}`}
              style={s.connList}
              renderItem={({ item }) => (
                <ConnRow
                  conn={item}
                  isConnecting={conn?.host === item.host && conn?.port === item.port && !tunnelError}
                  error={conn?.host === item.host && conn?.port === item.port ? tunnelError : null}
                  onSelect={() => selectConn(item)}
                  onDelete={() => deleteConn(item)}
                />
              )}
            />
            <View style={s.pickerFooter}>
              <TouchableOpacity style={[s.setupBtn, s.pickerScanBtn]} onPress={requestCameraAndScan}>
                <Text style={s.setupBtnText}>Scan new QR code</Text>
              </TouchableOpacity>
            </View>
          </View>
        )}
      </SafeAreaView>
    )
  }

  // ── Main UI ────────────────────────────────────────────────────────────────
  return (
    <SafeAreaView style={s.safe} edges={['top']}>
      <KeyboardAvoidingView
        style={s.paneArea}
        behavior={Platform.OS === 'ios' ? 'padding' : 'height'}
        keyboardVerticalOffset={0}
      >
        {/* Header */}
        <View style={s.header}>
          <View style={s.headerLeft}>
            <View>
              <Text style={s.headerTitle}>claudulhu</Text>
              {repoName && <Text style={s.headerRepo}>{repoName}</Text>}
            </View>
          </View>
          <View style={s.headerRight}>
            <TouchableOpacity style={s.iconBtn} onPress={() => setShowBranches(true)}>
              <Text style={s.iconBtnText}>⎇ {activeWorktrees}</Text>
            </TouchableOpacity>
            <TouchableOpacity style={s.iconBtn} onPress={() => setConn(null)}>
              <Text style={s.iconBtnText}>⬡</Text>
            </TouchableOpacity>
          </View>
        </View>

        {/* Tab bar */}
        <ScrollView horizontal style={s.tabBar} contentContainerStyle={s.tabBarInner} showsHorizontalScrollIndicator={false}>
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
                <TouchableOpacity onPress={() => closeTab(tab.id)} hitSlop={{ top: 8, bottom: 8, left: 6, right: 6 }}>
                  <Text style={s.tabClose}>×</Text>
                </TouchableOpacity>
              )}
            </TouchableOpacity>
          ))}
        </ScrollView>

        {/* Chat panes — all mounted, only active one visible */}
        {tabs.map(tab => (
          <View key={tab.id} style={tab.id === activeTab ? s.paneVisible : s.paneHidden}>
            <ChatPane
              wsUrl={tab.wsUrl}
              tunnelPort={tunnelPort}
              branches={branches}
              canSpawnWorker={tab.id === 'main'}
              onStatusChange={handleStatusChange(tab.id)}
              onWorkerCreated={handleWorkerCreated}
              initialMessage={tab.initialMessage}
            />
          </View>
        ))}
      </KeyboardAvoidingView>

      {/* Branches bottom sheet */}
      <Modal visible={showBranches} animationType="slide" transparent onRequestClose={() => setShowBranches(false)}>
        <Pressable style={s.overlay} onPress={() => setShowBranches(false)}>
          <Pressable style={s.sheet} onPress={e => e.stopPropagation()}>
            <View style={s.sheetHandle} />
            <View style={s.sheetHeader}>
              <Text style={s.sheetTitle}>worktrees</Text>
              <Text style={s.sheetCount}>{activeWorktrees}/{branches.length}</Text>
            </View>
            <FlatList
              data={branches}
              keyExtractor={b => b.name}
              ListEmptyComponent={<Text style={s.branchEmpty}>no branches found</Text>}
              renderItem={({ item: b }) => (
                <TouchableOpacity
                  style={s.branchRow}
                  onPress={() => {
                    if (!b.worktree) { return }
                    openTab(b.name, b.worktree)
                    setShowBranches(false)
                  }}
                  activeOpacity={b.worktree ? 0.7 : 1}
                >
                  <View style={[s.branchDot, { backgroundColor: b.worktree ? C.green : C.textMuted }]} />
                  <View style={s.branchInfo}>
                    <Text style={s.branchName}>{b.name}</Text>
                    <Text style={s.branchCommit}>{b.commit}</Text>
                    {b.worktree && <Text style={s.branchPath} numberOfLines={1}>{b.worktree}</Text>}
                  </View>
                  {b.worktree && (
                    <Text style={s.branchHint}>{openTabIds.has(b.name) ? 'open' : 'chat'}</Text>
                  )}
                </TouchableOpacity>
              )}
            />
          </Pressable>
        </Pressable>
      </Modal>
    </SafeAreaView>
  )
}

export default function App() {
  return (
    <SafeAreaProvider>
      <StatusBar barStyle="light-content" backgroundColor={C.bg} />
      <AppInner />
    </SafeAreaProvider>
  )
}

// ── Styles ────────────────────────────────────────────────────────────────────

const MONO = Platform.OS === 'ios' ? 'Menlo' : 'monospace'

const s = StyleSheet.create({
  // Setup / connecting screen
  setupSafe:        { flex: 1, backgroundColor: '#EB4F0B' },
  setupCenter:      { flex: 1, alignItems: 'center', justifyContent: 'center', paddingHorizontal: 40, gap: 16 },
  setupMark:        { fontSize: 48, color: '#fff' },
  setupTitle:       { fontSize: 26, fontWeight: '700', color: '#fff', letterSpacing: 2 },
  setupDesc:        { fontSize: 15, color: 'rgba(255,255,255,0.85)', textAlign: 'center', lineHeight: 22 },
  setupStatus:      { fontSize: 15, color: 'rgba(255,255,255,0.7)', textAlign: 'center' },
  setupError:       { fontSize: 14, color: '#ffe0d6', textAlign: 'center', lineHeight: 20 },
  setupBtn:         { backgroundColor: '#fff', borderRadius: 12, paddingVertical: 14, paddingHorizontal: 32, alignItems: 'center', marginTop: 8 },
  setupBtnText:     { color: '#EB4F0B', fontWeight: '700', fontSize: 16 },

  // Connection picker
  pickerWrap:       { flex: 1, backgroundColor: '#EB4F0B' },
  pickerHeader:     { alignItems: 'center', paddingTop: 48, paddingBottom: 24, gap: 8 },
  pickerFooter:     { paddingHorizontal: 24, paddingBottom: 24, paddingTop: 16 },
  pickerScanBtn:    { width: '100%' },
  connList:         { flex: 1 },
  connRow:          { flexDirection: 'row', alignItems: 'center', paddingHorizontal: 24, paddingVertical: 16, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: 'rgba(255,255,255,0.25)' },
  connInfo:         { flex: 1 },
  connLabel:        { color: '#fff', fontSize: 16, fontWeight: '600' },
  connHost:         { color: 'rgba(255,255,255,0.7)', fontSize: 12, marginTop: 2 },
  connError:        { color: '#ffe0d6', fontSize: 12, marginTop: 4 },
  connDelete:       { color: 'rgba(255,255,255,0.6)', fontSize: 18, paddingLeft: 16 },

  // QR scanner
  creatureImg:       { width: 120, height: 120, borderRadius: 26, marginBottom: 12 },

  scannerFull:      { ...StyleSheet.absoluteFillObject, backgroundColor: '#000', zIndex: 100 },
  scannerOverlay:   { ...StyleSheet.absoluteFillObject, alignItems: 'center', justifyContent: 'space-between', paddingVertical: 60 },
  scannerTopBar:    { alignItems: 'center', gap: 8, paddingHorizontal: 32 },
  scannerTitle:     { color: '#fff', fontSize: 20, fontWeight: '700' },
  scannerSubtitle:  { color: 'rgba(255,255,255,0.6)', fontSize: 14, textAlign: 'center', lineHeight: 20 },
  scannerReticle:   { width: 240, height: 240 },
  scannerCorner:    { position: 'absolute', width: 28, height: 28, borderColor: C.accent, borderWidth: 3 },
  cornerTL:         { top: 0, left: 0, borderRightWidth: 0, borderBottomWidth: 0, borderTopLeftRadius: 4 },
  cornerTR:         { top: 0, right: 0, borderLeftWidth: 0, borderBottomWidth: 0, borderTopRightRadius: 4 },
  cornerBL:         { bottom: 0, left: 0, borderRightWidth: 0, borderTopWidth: 0, borderBottomLeftRadius: 4 },
  cornerBR:         { bottom: 0, right: 0, borderLeftWidth: 0, borderTopWidth: 0, borderBottomRightRadius: 4 },
  scannerCancel:    { backgroundColor: 'rgba(255,255,255,0.15)', borderRadius: 24, paddingVertical: 12, paddingHorizontal: 32 },
  scannerCancelText:{ color: '#fff', fontSize: 16, fontWeight: '600' },
  scannerError:     { color: C.red, fontSize: 16, textAlign: 'center', marginBottom: 24 },

  // Layout
  safe:             { flex: 1, backgroundColor: C.bg },
  paneArea:         { flex: 1 },
  paneVisible:      { flex: 1 },
  paneHidden:       { flex: 1, display: 'none' },

  // Header
  header:           { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: 16, paddingVertical: 11, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  headerLeft:       { flexDirection: 'row', alignItems: 'center', gap: 8 },
  headerRight:      { flexDirection: 'row', gap: 8 },
  headerMark:       { fontSize: 20, color: C.accent },
  headerTitle:      { fontSize: 17, fontWeight: '700', color: C.textPrimary, letterSpacing: 1 },
  headerRepo:       { fontSize: 11, color: C.textSecondary, marginTop: 1 },
  iconBtn:          { backgroundColor: C.surface, borderWidth: StyleSheet.hairlineWidth, borderColor: C.border, borderRadius: 7, paddingHorizontal: 10, paddingVertical: 6 },
  iconBtnText:      { color: C.textSecondary, fontSize: 13 },

  // Tab bar
  tabBar:           { flexGrow: 0, maxHeight: 42, backgroundColor: C.surface, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  tabBarInner:      { alignItems: 'center', paddingHorizontal: 8, gap: 2 },
  tab:              { flexDirection: 'row', alignItems: 'center', paddingHorizontal: 10, paddingVertical: 9, borderRadius: 6, gap: 6, marginVertical: 4 },
  tabActive:        { backgroundColor: C.bg },
  tabDot:           { width: 6, height: 6, borderRadius: 3 },
  tabLabel:         { color: C.textMuted, fontSize: 13, maxWidth: 110 },
  tabLabelActive:   { color: C.textPrimary },
  tabClose:         { color: C.textMuted, fontSize: 16, lineHeight: 17, marginLeft: 2 },

  // Chat pane
  pane:             { flex: 1, backgroundColor: C.bg },
  messageList:      { flex: 1 },
  messageListContent: { paddingVertical: 16, paddingBottom: 8 },
  emptyState:       { textAlign: 'center', color: C.textMuted, fontSize: 14, marginTop: 80 },

  // Messages
  messageWrap:      { paddingHorizontal: 14, marginBottom: 14 },
  messageWrapRight: { alignItems: 'flex-end' },
  messageLabel:     { fontSize: 11, color: C.textMuted, marginBottom: 4, marginLeft: 2, fontWeight: '600', letterSpacing: 0.5, textTransform: 'uppercase' },
  messageLabelRight:{ marginLeft: 0, marginRight: 2 },

  cursor:           { color: C.accent, fontSize: 14 },

  // Text blocks
  textBlock:        { color: C.textPrimary, fontSize: 17, lineHeight: 26 },
  errorText:        { color: C.red, fontSize: 16 },
  mutedText:        { color: C.textMuted, fontSize: 15, fontStyle: 'italic' },
  systemText:       { color: C.textSecondary, fontSize: 15 },

  // Result footer
  resultFooter:     { flexDirection: 'row', justifyContent: 'space-between', paddingTop: 6, marginTop: 4, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: C.border },
  resultMeta:       { color: C.textMuted, fontSize: 12 },

  // Question
  questionRow:      { flexDirection: 'row', gap: 8, alignItems: 'flex-start' },
  questionMark:     { color: C.yellow, fontWeight: '700', fontSize: 15 },
  questionText:     { color: C.textPrimary, fontSize: 15, flex: 1, lineHeight: 22 },

  // Worker created
  workerRow:        { flexDirection: 'row', gap: 8, alignItems: 'flex-start' },
  workerIcon:       { color: C.accent, fontSize: 15 },
  workerInfo:       { flex: 1 },
  workerBranch:     { color: C.accent, fontSize: 14, fontWeight: '600' },
  workerPath:       { color: C.textMuted, fontSize: 11, fontFamily: MONO, marginTop: 2 },

  // Tool blocks
  toolLine:         { color: C.textMuted, fontSize: 12, fontFamily: MONO, marginTop: 4 },
  monoText:         { color: C.textSecondary, fontSize: 12, fontFamily: MONO, lineHeight: 18 },

  // Completions
  completionList:     { backgroundColor: C.surface, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: C.border, maxHeight: 160 },
  completionItem:     { paddingHorizontal: 14, paddingVertical: 10, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  completionText:     { color: C.textSecondary, fontSize: 13, fontFamily: MONO },

  // Input
  inputRow:         { flexDirection: 'row', alignItems: 'flex-end', paddingHorizontal: 12, paddingVertical: 10, paddingBottom: Platform.OS === 'android' ? 14 : 10, gap: 8, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: C.border, backgroundColor: C.surface },
  input:            { flex: 1, backgroundColor: C.inputBg, borderWidth: 1, borderColor: C.inputBorder, borderRadius: 12, paddingHorizontal: 14, paddingVertical: 12, color: C.textPrimary, fontSize: 17, lineHeight: 24, maxHeight: 140 },
  btnSend:          { width: 40, height: 40, backgroundColor: C.accent, borderRadius: 10, alignItems: 'center', justifyContent: 'center' },
  btnSendText:      { color: '#fff', fontSize: 15 },
  btnStop:          { width: 40, height: 40, backgroundColor: C.red, borderRadius: 10, alignItems: 'center', justifyContent: 'center' },
  btnStopText:      { color: '#fff', fontSize: 13 },
  btnDisabled:      { opacity: 0.3 },

  // Branches modal
  overlay:          { flex: 1, justifyContent: 'flex-end', backgroundColor: 'rgba(0,0,0,0.55)' },
  sheet:            { backgroundColor: C.surface, borderTopLeftRadius: 18, borderTopRightRadius: 18, maxHeight: '65%', paddingBottom: 32 },
  sheetHandle:      { width: 38, height: 4, backgroundColor: C.borderLight, borderRadius: 2, alignSelf: 'center', marginTop: 10, marginBottom: 2 },
  sheetHeader:      { flexDirection: 'row', justifyContent: 'space-between', alignItems: 'center', paddingHorizontal: 20, paddingVertical: 14, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  sheetTitle:       { color: C.textPrimary, fontSize: 16, fontWeight: '600' },
  sheetCount:       { color: C.textMuted, fontSize: 14 },
  branchEmpty:      { color: C.textMuted, textAlign: 'center', padding: 24, fontSize: 14 },
  branchRow:        { flexDirection: 'row', alignItems: 'center', paddingHorizontal: 20, paddingVertical: 14, gap: 12, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  branchDot:        { width: 8, height: 8, borderRadius: 4 },
  branchInfo:       { flex: 1 },
  branchName:       { color: C.textPrimary, fontSize: 15, fontWeight: '500' },
  branchCommit:     { color: C.textMuted, fontSize: 12, fontFamily: MONO, marginTop: 1 },
  branchPath:       { color: C.textMuted, fontSize: 11, fontFamily: MONO, marginTop: 2 },
  branchHint:       { color: C.accent, fontSize: 12 },
})
