import AsyncStorage from '@react-native-async-storage/async-storage'
import React, { useCallback, useEffect, memo, useRef, useState } from 'react'
import {
  ActivityIndicator,
  Animated,
  AppState,
  FlatList,
  Platform,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  TouchableOpacity,
  View,
} from 'react-native'
import { KeyboardProvider, useReanimatedKeyboardAnimation } from 'react-native-keyboard-controller'
import Reanimated, { useAnimatedStyle } from 'react-native-reanimated'
import { SafeAreaProvider, SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context'
import { Camera, useCameraDevice, useCodeScanner } from 'react-native-vision-camera'
import { PermissionsAndroid } from 'react-native'
import NoiseConnection from './src/NativeNoiseConnection'

// ── Types ──────────────────────────────────────────────────────────────────────

interface NoiseConnectionInfo {
  v:      number
  host:   string
  port:   number
  pk:     string
  label?: string
}

type ConnStatus = 'connecting' | 'ready' | 'streaming' | 'error'

interface ContainerInfo {
  id:      string
  name:    string
  git_url: string
  status:  string
  host:    string
  port:    number
  pubkey:  string
}

interface Message {
  id:    string
  role:  'user' | 'assistant' | 'tool' | 'session' | 'interrupted'
  text:  string
  cost?: number
}

// ── Helpers ────────────────────────────────────────────────────────────────────

let _id = 0
const uid = () => `m${Date.now()}_${++_id}`

const formatCost = (usd: number) =>
  usd < 0.01 ? `$${usd.toFixed(4)}` : `$${usd.toFixed(2)}`

function parseQrData(raw: string): NoiseConnectionInfo | null {
  const parts = raw.split(':')
  if (parts[0] === '2' && parts.length === 4) {
    const [, host, portStr, pk] = parts
    const port = parseInt(portStr, 10)
    if (!host || isNaN(port) || !pk) return null
    return { v: 2, host, port, pk }
  }
  return null
}

const connKeyFor = (c: NoiseConnectionInfo) => `${c.host}:${c.port}:${c.pk.slice(0, 8)}`

// ── Dev connection ─────────────────────────────────────────────────────────────
// Fixed dev keypair baked into the server when CLAUDULHU_DEV=1.
// Public key (base32): 34577VOSZRDRTUB7XYTT6FS62Y4QYYVLQJCHP4XNDQA2763AU5YQ
//
// iOS Simulator shares the Mac's network stack — 127.0.0.1 reaches Docker's
// bound port directly.
const DEV_HOST = '127.0.0.1'
const DEV_CONN: NoiseConnectionInfo = {
  v:     2,
  host:  DEV_HOST,
  port:  9000,
  pk:    '34577VOSZRDRTUB7XYTT6FS62Y4QYYVLQJCHP4XNDQA2763AU5YQ',
  label: 'dev (local)',
}

// ── Fonts ──────────────────────────────────────────────────────────────────────

const ARIMO = 'Arimo'

// ── Colours ────────────────────────────────────────────────────────────────────

const C = {
  bg:            '#ffffff',
  surface:       '#f2f2f7',
  border:        '#d1d1d6',
  accent:        '#2563eb',
  green:         '#22863a',
  yellow:        '#b45309',
  red:           '#dc2626',
  textPrimary:   '#1c1c1e',
  textSecondary: '#6b6b6b',
  textMuted:     '#aeaeb2',
  inputBorder:   '#d1d1d6',
}

const statusColor = (st: ConnStatus): string => {
  if (st === 'ready')     return C.green
  if (st === 'streaming') return C.accent
  if (st === 'error')     return C.red
  return C.yellow
}

// ── Text rendering ─────────────────────────────────────────────────────────────

function renderInlineSegment(text: string, baseStyle: object, key: number) {
  const parts = text.split(/\*\*(.+?)\*\*/gs)
  if (parts.length === 1) return <Text key={key} style={baseStyle}>{text}</Text>
  return (
    <Text key={key} style={baseStyle}>
      {parts.map((part, i) =>
        i % 2 === 1
          ? <Text key={i} style={{ fontWeight: '900' }}>{part}</Text>
          : part
      )}
    </Text>
  )
}

function renderText(text: string, baseStyle: object) {
  if (!text) return null
  const blocks = text.split(/(```[\s\S]*?```)/g)
  const elements: React.ReactNode[] = []
  blocks.forEach((block, bi) => {
    if (block.startsWith('```') && block.endsWith('```')) {
      const inner = block.slice(3, -3).replace(/^\n/, '')
      elements.push(
        <View key={bi} style={s.codeBlock}>
          <Text style={s.codeBlockText}>{inner}</Text>
        </View>
      )
    } else {
      const inlineParts = block.split(/`([^`]+)`/g)
      if (inlineParts.length === 1) {
        elements.push(renderInlineSegment(block, baseStyle, bi))
      } else {
        elements.push(
          <Text key={bi} style={baseStyle}>
            {inlineParts.map((part, i) =>
              i % 2 === 1
                ? <Text key={i} style={s.inlineCode}>{part}</Text>
                : (() => {
                    const boldParts = part.split(/\*\*(.+?)\*\*/gs)
                    if (boldParts.length === 1) return part
                    return boldParts.map((bp, j) =>
                      j % 2 === 1
                        ? <Text key={j} style={{ fontWeight: '900' }}>{bp}</Text>
                        : bp
                    )
                  })()
            )}
          </Text>
        )
      }
    }
  })
  return <>{elements}</>
}

// ── PendingEllipsis ───────────────────────────────────────────────────────────

function PendingEllipsis() {
  const dots = [useRef(new Animated.Value(0)).current, useRef(new Animated.Value(0)).current, useRef(new Animated.Value(0)).current]
  useEffect(() => {
    const anims = dots.map((dot, i) =>
      Animated.loop(Animated.sequence([
        Animated.delay(i * 150),
        Animated.timing(dot, { toValue: 1, duration: 300, useNativeDriver: true }),
        Animated.timing(dot, { toValue: 0, duration: 300, useNativeDriver: true }),
        Animated.delay((dots.length - i - 1) * 150),
      ]))
    )
    anims.forEach(a => a.start())
    return () => anims.forEach(a => a.stop())
  }, [])
  return (
    <View style={s.messageWrap}>
      <View style={{ flexDirection: 'row', alignItems: 'center', gap: 4 }}>
        {dots.map((dot, i) => (
          <Animated.Text key={i} style={[s.cursor, { opacity: dot }]}>●</Animated.Text>
        ))}
      </View>
    </View>
  )
}

// ── @ completion helpers ───────────────────────────────────────────────────────

function parseAtQuery(text: string): { atIndex: number; dirPart: string; filePart: string } | null {
  const atIndex = text.lastIndexOf('@')
  if (atIndex === -1) return null
  const query = text.slice(atIndex + 1)
  if (query.includes(' ')) return null
  const lastSlash = query.lastIndexOf('/')
  return lastSlash === -1
    ? { atIndex, dirPart: '', filePart: query }
    : { atIndex, dirPart: query.slice(0, lastSlash + 1), filePart: query.slice(lastSlash + 1) }
}

// ── Container display name ─────────────────────────────────────────────────────

function containerDisplayName(name: string): string {
  return name.replace(/^rulyeh-/, '')
}

// ── MessageBubble ─────────────────────────────────────────────────────────────

const MessageBubble = memo(function MessageBubble({ message }: { message: Message }) {
  if (message.role === 'session') {
    return null
  }
  if (message.role === 'interrupted') {
    return (
      <View style={[s.messageWrap, { marginBottom: 3, paddingLeft: 28 }]}>
        <Text style={s.interruptedLine}>■ interrupted</Text>
      </View>
    )
  }
  if (message.role === 'tool') {
    return (
      <View style={[s.messageWrap, { marginBottom: 3 }]}>
        <Text style={s.toolLine} numberOfLines={1} ellipsizeMode="tail">{message.text}</Text>
      </View>
    )
  }
  if (message.role === 'user') {
    return (
      <View style={[s.messageWrap, s.messageWrapRight]}>
        <View style={s.userBubble}>
          {renderText(message.text, s.textBlock)}
        </View>
      </View>
    )
  }
  return (
    <View style={s.messageWrap}>
      {renderText(message.text, s.textBlock)}
      {message.cost != null && (
        <Text style={s.costLabel}>{formatCost(message.cost)}</Text>
      )}
    </View>
  )
})

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
  const device      = useCameraDevice('back')
  const scannedRef  = useRef(false)
  const codeScanner = useCodeScanner({
    codeTypes: ['qr'],
    onCodeScanned: (codes) => {
      if (scannedRef.current) return
      const value = codes[0]?.value
      if (value) { scannedRef.current = true; onScanned(value) }
    },
  })
  if (!device) return (
    <View style={s.scannerFull}>
      <Text style={s.scannerError}>Camera not available</Text>
      <TouchableOpacity style={s.scannerCancel} onPress={onCancel}>
        <Text style={s.scannerCancelText}>Cancel</Text>
      </TouchableOpacity>
    </View>
  )
  return (
    <View style={s.scannerFull}>
      <Camera device={device} isActive codeScanner={codeScanner} style={StyleSheet.absoluteFill} />
      <View style={s.scannerOverlay}>
        <View style={s.scannerTopBar}>
          <Text style={s.scannerTitle}>Scan QR code</Text>
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

const ChatPane = memo(function ChatPane({
  baseUrl, onStatusChange, clearRef, initialDraft, onDraftChange,
}: {
  baseUrl:        string
  onStatusChange: (s: ConnStatus) => void
  clearRef:       React.MutableRefObject<() => void>
  initialDraft?:  string
  onDraftChange?: (draft: string) => void
}) {
  const insets                     = useSafeAreaInsets()
  const { height: keyboardHeight } = useReanimatedKeyboardAnimation()
  const spacerStyle                = useAnimatedStyle(() => ({
    height: Math.max(insets.bottom, -keyboardHeight.value),
  }))

  const [messages,      setMessages]      = useState<Message[]>([])
  const [status,        setStatus]        = useState<ConnStatus>('connecting')
  const [input,         setInput]         = useState(initialDraft ?? '')
  const draftKey = `draft:${baseUrl}`
  const [completions,   setCompletions]   = useState<string[]>([])
  const [showScrollBtn, setShowScrollBtn] = useState(false)
  const [inputAreaH,    setInputAreaH]    = useState(0)

  const sendMessageRef    = useRef<() => void>(() => {})
  const wsRef             = useRef<WebSocket | null>(null)
  const listRef           = useRef<FlatList<Message>>(null)
  const isAtBottomRef     = useRef(true)
  const contentHeightRef  = useRef(0)
  const listHeightRef     = useRef(0)

  const updateStatus = useCallback((s: ConnStatus) => {
    setStatus(s)
    onStatusChange(s)
  }, [onStatusChange])

  const loadHistory = useCallback((costForLast?: number) => {
    fetch(`${baseUrl}/history`)
      .then(r => r.json())
      .then((data: { messages: Array<{ role: string; text: string }> }) => {
        const msgs: Message[] = data.messages.map((m, i) => ({
          id:   `h${i}`,
          role: m.role as Message['role'],
          text: m.text,
        }))
        if (costForLast != null) {
          for (let i = msgs.length - 1; i >= 0; i--) {
            if (msgs[i].role === 'assistant') { msgs[i] = { ...msgs[i], cost: costForLast }; break }
          }
        }
        setMessages(msgs)
        updateStatus('ready')
        setTimeout(() => {
          const offset = Math.max(0, contentHeightRef.current - listHeightRef.current)
          listRef.current?.scrollToOffset({ offset, animated: false })
        }, 50)
      })
      .catch(() => updateStatus('error'))
  }, [baseUrl])

  // Restore draft input on mount / baseUrl change.
  // Restore draft on mount / baseUrl change (cold-start fallback; skipped if
  // the parent already provided initialDraft from its in-memory cache).
  useEffect(() => {
    if (initialDraft != null) return
    AsyncStorage.getItem(draftKey).then(v => { if (v != null) setInput(v) }).catch(() => {})
  }, [draftKey])

  // Persist draft on every change.
  useEffect(() => {
    AsyncStorage.setItem(draftKey, input).catch(() => {})
    onDraftChange?.(input)
  }, [draftKey, input])

  // Fetch history on mount and when baseUrl changes.
  useEffect(() => {
    updateStatus('connecting')
    loadHistory()
  }, [baseUrl])

  // Re-fetch history when app foregrounds (tunnel may have reconnected).
  useEffect(() => {
    const sub = AppState.addEventListener('change', nextState => {
      if (nextState === 'active') loadHistory()
    })
    return () => sub.remove()
  }, [loadHistory])

  // @ completions
  useEffect(() => {
    const parsed = parseAtQuery(input)
    if (!parsed) { setCompletions([]); return }
    let cancelled = false
    fetch(`${baseUrl}/completions?dir_part=${encodeURIComponent(parsed.dirPart)}&file_part=${encodeURIComponent(parsed.filePart)}`)
      .then(r => r.json())
      .then((data: string[]) => { if (!cancelled) setCompletions(data) })
      .catch(() => { if (!cancelled) setCompletions([]) })
    return () => { cancelled = true }
  }, [input, baseUrl])

  const applyCompletion = useCallback((completion: string) => {
    const parsed = parseAtQuery(input)
    if (!parsed) return
    const newText = input.slice(0, parsed.atIndex + 1) + completion
    if (completion.endsWith('/')) {
      setInput(newText)
    } else {
      setInput(newText + ' ')
      setCompletions([])
    }
  }, [input])

  const sendMessage = useCallback(() => {
    const text = input.trim()
    if (!text || status === 'streaming') return

    setMessages(prev => [...prev, { id: uid(), role: 'user' as const, text }])
    isAtBottomRef.current = true
    setInput('')
    AsyncStorage.removeItem(draftKey).catch(() => {})
    updateStatus('streaming')

    let streamingId = uid()
    let hasAssistantMsg = false

    const handleEvent = (raw: string) => {
      let event: { type: string; text?: string; tool?: string; input?: unknown; cost_usd?: number; message?: string }
      try { event = JSON.parse(raw) } catch { return }

      if (event.type === 'text' && event.text) {
        const chunk = event.text
        if (!hasAssistantMsg) {
          hasAssistantMsg = true
          setMessages(prev => [...prev, { id: streamingId, role: 'assistant' as const, text: chunk }])
        } else {
          setMessages(prev => prev.map(m => m.id === streamingId ? { ...m, text: m.text + chunk } : m))
        }
      } else if (event.type === 'tool_use') {
        hasAssistantMsg = false
        streamingId = uid()
        const firstVal = event.input && typeof event.input === 'object'
          ? String(Object.values(event.input as Record<string, unknown>)[0] ?? '').trim().slice(0, 60)
          : ''
        const toolText = firstVal ? `${event.tool}(${firstVal})` : (event.tool ?? '')
        setMessages(prev => [...prev, { id: uid(), role: 'tool' as const, text: toolText }])
      } else if (event.type === 'done') {
        wsRef.current = null
        loadHistory(event.cost_usd)
      } else if (event.type === 'interrupted') {
        wsRef.current = null
        loadHistory(event.cost_usd)
      } else if (event.type === 'error') {
        wsRef.current = null
        setMessages(prev => [...prev, { id: uid(), role: 'assistant' as const, text: `\u2717 ${event.message ?? 'error'}` }])
        updateStatus('ready')
      }
    }

    const wsUrl = baseUrl.replace(/^http/, 'ws') + '/stream'
    const ws = new WebSocket(wsUrl)
    wsRef.current = ws
    ws.onopen = () => { ws.send(JSON.stringify({ text })) }
    ws.onmessage = (e) => { handleEvent(e.data) }
    ws.onerror = () => {
      wsRef.current = null
      setMessages(prev => [...prev, { id: uid(), role: 'assistant' as const, text: '\u2717 network error' }])
      updateStatus('error')
    }
  }, [input, status, baseUrl, loadHistory])

  sendMessageRef.current = sendMessage

  const clearConversation = useCallback(() => {
    fetch(`${baseUrl}/clear`, { method: 'POST' }).catch(() => {})
    setMessages([])
    updateStatus('ready')
  }, [baseUrl])
  clearRef.current = clearConversation

  const isPending = status === 'streaming'

  return (
    <View style={s.pane}>
      <View style={{ flex: 1 }}>
        <FlatList
          ref={listRef}
          data={messages}
          keyExtractor={m => m.id}
          renderItem={({ item }) => <MessageBubble message={item} />}
          contentContainerStyle={[s.messageListContent, { paddingBottom: inputAreaH + 8 }]}
          style={s.messageList}
          ListEmptyComponent={<Text style={s.emptyState}>say something</Text>}
          onContentSizeChange={(_, h) => {
            contentHeightRef.current = h
            if (isAtBottomRef.current) {
              const offset = Math.max(0, h - listHeightRef.current)
              listRef.current?.scrollToOffset({ offset, animated: false })
            }
          }}
          onLayout={e => { listHeightRef.current = e.nativeEvent.layout.height }}
          onScroll={({ nativeEvent: { layoutMeasurement, contentOffset, contentSize } }) => {
            const atBottom = contentOffset.y + layoutMeasurement.height >= contentSize.height - 80
            if (atBottom !== isAtBottomRef.current) {
              isAtBottomRef.current = atBottom
              setShowScrollBtn(!atBottom)
            }
          }}
          scrollEventThrottle={16}
          keyboardShouldPersistTaps="handled"
          automaticallyAdjustKeyboardInsets={false}
          ListFooterComponent={isPending ? <PendingEllipsis /> : null}
        />

        {status === 'error' && (
          <View style={s.reconnectBanner}>
            <Text style={s.reconnectText}>connection error</Text>
          </View>
        )}

        <View style={s.inputFloat} onLayout={e => setInputAreaH(e.nativeEvent.layout.height)}>
          {completions.length > 0 && (
            <ScrollView
              style={s.completionList}
              keyboardShouldPersistTaps="handled"
              showsVerticalScrollIndicator={false}
            >
              {completions.map(c => (
                <TouchableOpacity key={c} style={s.completionItem} onPress={() => applyCompletion(c)}>
                  <Text style={s.completionText}>{c}</Text>
                </TouchableOpacity>
              ))}
            </ScrollView>
          )}
          {isPending ? (
            <TouchableOpacity
              style={s.inputStopBtn}
              onPress={() => {
                const ws = wsRef.current
                if (ws) {
                  ws.send(JSON.stringify({ type: 'interrupt' }))
                }
              }}
              activeOpacity={0.7}
            >
              <Text style={s.stopBtnText}>■  stop</Text>
            </TouchableOpacity>
          ) : (
            <TextInput
              style={s.input}
              value={input}
              onChangeText={text => {
                if (text.includes('\n')) { sendMessageRef.current(); return }
                setInput(text)
              }}
              onSubmitEditing={() => sendMessageRef.current()}
              placeholder="message…"
              placeholderTextColor={C.textMuted}
              multiline
              returnKeyType="send"
              blurOnSubmit={false}
            />
          )}
        </View>

        {showScrollBtn && (
          <View style={[s.scrollBtnWrap, { bottom: inputAreaH }]} pointerEvents="box-none">
            <TouchableOpacity
              style={s.scrollBtn}
              onPress={() => {
                isAtBottomRef.current = true
                setShowScrollBtn(false)
                const offset = Math.max(0, contentHeightRef.current - listHeightRef.current)
                listRef.current?.scrollToOffset({ offset, animated: true })
              }}
              activeOpacity={0.75}
            >
              <Text style={s.scrollBtnIcon}>↓</Text>
            </TouchableOpacity>
          </View>
        )}
      </View>
      <Reanimated.View style={[{ backgroundColor: C.surface }, spacerStyle]} />
    </View>
  )
})


// ── ChildChatScreen ───────────────────────────────────────────────────────────

function ChildChatScreen({ child, tunnelPort, tunnelError, onClose, initialDraft, onDraftChange }: {
  child:          ContainerInfo
  tunnelPort:     number | null
  tunnelError:    string | null
  onClose:        () => void
  initialDraft?:  string
  onDraftChange?: (draft: string) => void
}) {
  const [chatStatus, setChatStatus] = useState<ConnStatus>('connecting')
  const clearRef = useRef<() => void>(() => {})

  return (
    <SafeAreaView style={s.safe} edges={['top']}>
      <View style={s.paneArea}>
        <View style={s.header}>
          <View style={s.headerLeft}>
            <TouchableOpacity
              style={s.backBtn}
              onPress={onClose}
              hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
            >
              <Text style={s.backBtnText}>‹</Text>
            </TouchableOpacity>
            <View style={[s.connDot, { backgroundColor: statusColor(chatStatus) }]} />
            <View>
              <Text style={s.headerTitle}>{containerDisplayName(child.name)}</Text>
            </View>
          </View>
          <TouchableOpacity
            style={s.clearBtn}
            onPress={() => clearRef.current()}
            hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
            disabled={chatStatus !== 'ready'}
          >
            <Text style={[s.clearBtnText, chatStatus !== 'ready' && { opacity: 0.3 }]}>clear</Text>
          </TouchableOpacity>
        </View>

        {tunnelPort ? (
          <ChatPane
            baseUrl={`http://127.0.0.1:${tunnelPort}`}
            onStatusChange={setChatStatus}
            clearRef={clearRef}
            initialDraft={initialDraft}
            onDraftChange={onDraftChange}
          />
        ) : (
          <View style={s.setupCenter}>
            {tunnelError
              ? <Text style={[s.setupError, { color: C.red }]}>{tunnelError}</Text>
              : <ActivityIndicator color={C.accent} size="small" />
            }
          </View>
        )}
      </View>
    </SafeAreaView>
  )
}


// ── ErrorBoundary ─────────────────────────────────────────────────────────────

class ErrorBoundary extends React.Component<
  { children: React.ReactNode },
  { error: string | null }
> {
  constructor(props: { children: React.ReactNode }) {
    super(props)
    this.state = { error: null }
  }
  static getDerivedStateFromError(e: unknown) {
    return { error: e instanceof Error ? e.message : String(e) }
  }
  render() {
    if (this.state.error) {
      return (
        <View style={{ flex: 1, backgroundColor: '#EB4F0B', alignItems: 'center', justifyContent: 'center', padding: 32 }}>
          <Text style={{ color: '#fff', fontSize: 18, fontWeight: '700', marginBottom: 12 }}>Something went wrong</Text>
          <Text style={{ color: 'rgba(255,255,255,0.85)', fontSize: 14, textAlign: 'center', lineHeight: 20 }}>{this.state.error}</Text>
        </View>
      )
    }
    return this.props.children
  }
}

// ── AppInner ──────────────────────────────────────────────────────────────────

function AppInner() {
  const [conn,        setConn]        = useState<NoiseConnectionInfo | null>(null)
  const [tunnelPort,  setTunnelPort]  = useState<number | null>(null)
  const [tunnelError, setTunnelError] = useState<string | null>(null)
  const [scanning,    setScanning]    = useState(false)
  const [chatStatus,  setChatStatus]  = useState<ConnStatus>('connecting')
  const [containers,          setContainers]          = useState<ContainerInfo[]>([])
  const [activeChild,         setActiveChild]         = useState<ContainerInfo | null>(null)
  const [showSettingsMenu,    setShowSettingsMenu]    = useState(false)
  const [startingContainerId, setStartingContainerId] = useState<string | null>(null)
  const [startingError,       setStartingError]       = useState<string | null>(null)
  const startingContainerIdRef = useRef<string | null>(null)
  const [reconnectKey, setReconnectKey] = useState(0)
  const clearChatRef = useRef<() => void>(() => {})
  // In-memory draft cache: survives ChatPane unmount/remount without async latency.
  const draftsRef = useRef<Record<string, string>>({})

  // masterBaseUrl is only valid when not viewing a child — fetching containers
  // and sending master messages must always go through the master tunnel.
  const masterBaseUrl = !activeChild && tunnelPort ? `http://127.0.0.1:${tunnelPort}` : null

  // Load saved master connection on mount and auto-connect.
  useEffect(() => {
    let cancelled = false
    const load = async () => {
      let saved: NoiseConnectionInfo | null = null
      const json = await AsyncStorage.getItem('masterConnection').catch(() => null)
      if (json) { try { saved = JSON.parse(json) } catch {} }
      if (!saved && __DEV__) { saved = DEV_CONN }
      if (!cancelled && saved) setConn(saved)
    }
    load()
    return () => { cancelled = true }
  }, [])

  // Single connection effect — owns the entire Noise tunnel lifecycle.
  // Connects to whichever target is currently active: child if one is open,
  // master otherwise. On failure from a child, clears activeChild so the
  // effect re-runs immediately and falls back to the master connection.
  useEffect(() => {
    setTunnelPort(null)
    setTunnelError(null)

    const target = activeChild
      ? { host: activeChild.host, port: activeChild.port, pk: activeChild.pubkey }
      : conn
      ? { host: conn.host,        port: conn.port,        pk: conn.pk }
      : null

    if (!target) return
    if (!NoiseConnection) { setTunnelError('Native Noise module unavailable'); return }

    let live = true
    NoiseConnection.disconnect()
    NoiseConnection.connect(target.host, target.port, target.pk)
      .then(port => { if (live) setTunnelPort(port) })
      .catch(e => {
        if (!live) return
        if (activeChild) {
          setActiveChild(null) // fall back to master; re-triggers this effect
        } else {
          setTunnelError(e?.message ?? String(e))
        }
      })

    return () => { live = false; NoiseConnection?.disconnect() }
  }, [conn, activeChild, reconnectKey])

  // Single AppState listener — bumps reconnectKey to re-run the connection effect.
  useEffect(() => {
    const sub = AppState.addEventListener('change', state => {
      if (state === 'active') setReconnectKey(k => k + 1)
    })
    return () => sub.remove()
  }, [])

  // Fetch container list from rulyeh.
  const fetchContainers = useCallback(() => {
    if (!masterBaseUrl) return
    fetch(`${masterBaseUrl}/containers`)
      .then(r => r.json())
      .then((data: { containers: ContainerInfo[] }) => {
        setContainers(data.containers)
        // If we're waiting for a container to start, check if it's up now.
        const waitingId = startingContainerIdRef.current
        if (waitingId) {
          const started = data.containers.find(c => c.id === waitingId && c.status === 'running' && c.pubkey)
          if (started) {
            startingContainerIdRef.current = null
            setStartingContainerId(null)
            setStartingError(null)
            setActiveChild(started)
          }
        }
      })
      .catch(() => {})
  }, [masterBaseUrl])

  // Fetch containers on connect and periodically while a start is in progress.
  useEffect(() => {
    if (!masterBaseUrl) return
    fetchContainers()
  }, [masterBaseUrl])

  useEffect(() => {
    if (!startingContainerId) return
    const interval = setInterval(fetchContainers, 3000)
    return () => clearInterval(interval)
  }, [startingContainerId, fetchContainers])

  const handleQrScanned = useCallback((raw: string) => {
    setScanning(false)
    const parsed = parseQrData(raw)
    if (!parsed) { setTunnelError('Invalid QR code'); return }
    AsyncStorage.setItem('masterConnection', JSON.stringify(parsed)).catch(() => {})
    setConn(parsed)
  }, [])

  const requestCameraAndScan = useCallback(async () => {
    if (Platform.OS === 'android') {
      const granted = await PermissionsAndroid.request(PermissionsAndroid.PERMISSIONS.CAMERA)
      if (granted !== PermissionsAndroid.RESULTS.GRANTED) return
    }
    setScanning(true)
  }, [])

  const handleLogout = useCallback(async () => {
    setShowSettingsMenu(false)
    await AsyncStorage.clear().catch(() => {})
    NoiseConnection?.disconnect()
    setConn(null)
  }, [])

  const startContainer = useCallback((id: string) => {
    if (!masterBaseUrl) return
    startingContainerIdRef.current = id
    setStartingContainerId(id)
    setStartingError(null)
    fetch(`${masterBaseUrl}/containers/start`, {
      method:  'POST',
      headers: { 'Content-Type': 'application/json' },
      body:    JSON.stringify({ id }),
    })
      .then(r => r.json())
      .then((data: { error?: string }) => {
        if (data.error) {
          startingContainerIdRef.current = null
          setStartingContainerId(null)
          setStartingError(data.error)
        }
        // On success, the poll interval will detect the running container.
      })
      .catch(e => {
        startingContainerIdRef.current = null
        setStartingContainerId(null)
        setStartingError(String(e))
      })
  }, [masterBaseUrl])

  // ── QR scanner overlay ──────────────────────────────────────────────────────
  if (scanning) {
    return <QrScanner onScanned={handleQrScanned} onCancel={() => setScanning(false)} />
  }

  // ── Connecting screen ───────────────────────────────────────────────────────
  if (conn && !tunnelPort) {
    return (
      <SafeAreaView style={s.setupSafe} edges={['top', 'bottom']}>
        <View style={s.setupCenter}>
          <CreatureAnim />
          <Text style={s.setupTitle}>claudulhu</Text>
          {tunnelError
            ? <Text style={s.setupError}>{tunnelError}</Text>
            : <ActivityIndicator color="#fff" size="small" style={{ marginTop: 8 }} />
          }
          <TouchableOpacity style={s.setupBtn} onPress={() => setConn(null)}>
            <Text style={s.setupBtnText}>back</Text>
          </TouchableOpacity>
        </View>
      </SafeAreaView>
    )
  }

  // ── No master connection yet ─────────────────────────────────────────────────
  if (!conn) {
    return (
      <SafeAreaView style={s.setupSafe} edges={['top', 'bottom']}>
        <View style={s.setupCenter}>
          <CreatureAnim />
          <Text style={s.setupTitle}>claudulhu</Text>
          <Text style={s.setupDesc}>Scan your master container QR code to connect</Text>
          <TouchableOpacity style={s.setupBtn} onPress={requestCameraAndScan}>
            <Text style={s.setupBtnText}>Scan QR code</Text>
          </TouchableOpacity>
        </View>
      </SafeAreaView>
    )
  }

  // ── Child chat screen ───────────────────────────────────────────────────────
  if (activeChild) {
    const childKey = activeChild.id
    return (
      <ChildChatScreen
        child={activeChild}
        tunnelPort={tunnelPort}
        tunnelError={tunnelError}
        onClose={() => setActiveChild(null)}
        initialDraft={draftsRef.current[childKey]}
        onDraftChange={d => { draftsRef.current[childKey] = d }}
      />
    )
  }

  // ── Master chat UI ───────────────────────────────────────────────────────────
  return (
    <SafeAreaView style={s.safe} edges={['top']}>
      <View style={s.paneArea}>
        <View style={s.header}>
          <View style={s.headerLeft}>
            <View style={[s.connDot, { backgroundColor: statusColor(chatStatus) }]} />
            <Text style={s.headerTitle}>rulyeh</Text>
          </View>
          <View style={s.headerRight}>
            <TouchableOpacity
              style={s.clearBtn}
              onPress={() => clearChatRef.current()}
              hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
              disabled={chatStatus !== 'ready'}
            >
              <Text style={[s.clearBtnText, chatStatus !== 'ready' && { opacity: 0.3 }]}>clear</Text>
            </TouchableOpacity>
            <TouchableOpacity
              style={s.settingsMenuBtn}
              onPress={() => {
                fetchContainers()
                setShowSettingsMenu(v => !v)
              }}
              hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
            >
              <Text style={s.settingsMenuBtnText}>···</Text>
            </TouchableOpacity>
          </View>
        </View>

        {showSettingsMenu && (
          <View style={s.containerMenuWrap}>
            <TouchableOpacity
              style={StyleSheet.absoluteFillObject}
              activeOpacity={1}
              onPress={() => setShowSettingsMenu(false)}
            />
            <View style={s.containerMenu}>
              <View style={s.settingsMenuSection}>
                <Text style={s.settingsMenuSectionTitle}>repos</Text>
              </View>
              <ScrollView
                style={s.containerMenuScroll}
                bounces={false}
                keyboardShouldPersistTaps="handled"
                showsVerticalScrollIndicator={false}
              >
                {containers.length === 0 && (
                  <View style={s.containerMenuItem}>
                    <Text style={s.containerMenuItemStatus}>No containers</Text>
                  </View>
                )}
                {containers.map(c => (
                  <TouchableOpacity
                    key={c.id}
                    style={s.containerMenuItem}
                    onPress={() => {
                      setShowSettingsMenu(false)
                      if (c.status === 'running') {
                        setActiveChild(c)
                      } else {
                        startContainer(c.id)
                      }
                    }}
                    activeOpacity={0.7}
                  >
                    <View style={[s.containerDot, {
                      backgroundColor: c.status === 'running' ? C.green : C.textMuted,
                    }]} />
                    <View style={{ flex: 1 }}>
                      <Text style={s.containerMenuItemName}>{containerDisplayName(c.name)}</Text>
                      {c.git_url ? <Text style={s.containerMenuItemUrl} numberOfLines={1}>{c.git_url}</Text> : null}
                    </View>
                    <Text style={s.containerMenuItemStatus}>{c.status}</Text>
                  </TouchableOpacity>
                ))}
              </ScrollView>
              <View style={s.settingsMenuDivider} />
              <TouchableOpacity style={s.settingsMenuAction} onPress={handleLogout}>
                <Text style={s.settingsMenuLogoutText}>exit</Text>
              </TouchableOpacity>
            </View>
          </View>
        )}

        {masterBaseUrl && (
          <ChatPane
            baseUrl={masterBaseUrl}
            onStatusChange={setChatStatus}
            clearRef={clearChatRef}
            initialDraft={draftsRef.current['master']}
            onDraftChange={d => { draftsRef.current['master'] = d }}
          />
        )}

        {startingContainerId !== null && (
          <View style={s.startingOverlay}>
            {startingError ? (
              <>
                <Text style={s.startingErrorText}>Failed to start container</Text>
                <Text style={s.startingErrorDetail}>{startingError}</Text>
              </>
            ) : (
              <>
                <ActivityIndicator color={C.accent} size="large" />
                <Text style={s.startingText}>Starting container...</Text>
              </>
            )}
            <TouchableOpacity
              style={s.startingCancelBtn}
              onPress={() => {
                startingContainerIdRef.current = null
                setStartingContainerId(null)
                setStartingError(null)
              }}
            >
              <Text style={s.startingCancelText}>{startingError ? 'dismiss' : 'cancel'}</Text>
            </TouchableOpacity>
          </View>
        )}
      </View>
    </SafeAreaView>
  )
}

// ── App ────────────────────────────────────────────────────────────────────────

export default function App() {
  return (
    <ErrorBoundary>
      <KeyboardProvider>
        <SafeAreaProvider>
          <AppInner />
        </SafeAreaProvider>
      </KeyboardProvider>
    </ErrorBoundary>
  )
}

// ── Styles ────────────────────────────────────────────────────────────────────

const s = StyleSheet.create({
  // Setup / connecting / picker
  setupSafe:    { flex: 1, backgroundColor: '#EB4F0B' },
  setupCenter:  { flex: 1, alignItems: 'center', justifyContent: 'center', paddingHorizontal: 40, gap: 16 },
  setupTitle:   { fontSize: 26, fontWeight: '700', color: '#fff', letterSpacing: 2, fontFamily: ARIMO },
  setupDesc:    { fontSize: 15, color: 'rgba(255,255,255,0.85)', textAlign: 'center', lineHeight: 22, fontFamily: ARIMO },
  setupStatus:  { fontSize: 15, color: 'rgba(255,255,255,0.7)', textAlign: 'center', fontFamily: ARIMO },
  setupError:   { fontSize: 14, color: '#ffe0d6', textAlign: 'center', lineHeight: 20, fontFamily: ARIMO },
  setupBtn:     { backgroundColor: '#fff', borderRadius: 12, paddingVertical: 14, paddingHorizontal: 32, alignItems: 'center', marginTop: 8 },
  setupBtnText: { color: '#EB4F0B', fontWeight: '700', fontSize: 16, fontFamily: ARIMO },

  // QR scanner
  creatureImg:       { width: 120, height: 120, borderRadius: 26, marginBottom: 12 },
  startingOverlay:    { ...StyleSheet.absoluteFillObject, backgroundColor: C.bg, alignItems: 'center', justifyContent: 'center', gap: 16, paddingHorizontal: 32 },
  startingText:       { fontSize: 15, color: C.textSecondary, fontFamily: ARIMO },
  startingErrorText:  { fontSize: 16, fontWeight: '600', color: C.red, fontFamily: ARIMO, textAlign: 'center' },
  startingErrorDetail:{ fontSize: 13, color: C.textSecondary, fontFamily: ARIMO, textAlign: 'center', lineHeight: 18 },
  startingCancelBtn:  { marginTop: 8, paddingVertical: 10, paddingHorizontal: 28, borderRadius: 10, borderWidth: 1, borderColor: C.border },
  startingCancelText: { fontSize: 15, color: C.textPrimary, fontFamily: ARIMO },
  scannerFull:       { ...StyleSheet.absoluteFillObject, backgroundColor: '#000', zIndex: 100 },
  scannerOverlay:    { ...StyleSheet.absoluteFillObject, alignItems: 'center', justifyContent: 'space-between', paddingVertical: 60 },
  scannerTopBar:     { alignItems: 'center', gap: 8, paddingHorizontal: 32 },
  scannerTitle:      { color: '#fff', fontSize: 20, fontWeight: '700', fontFamily: ARIMO },
  scannerSubtitle:   { color: 'rgba(255,255,255,0.6)', fontSize: 14, textAlign: 'center', lineHeight: 20, fontFamily: ARIMO },
  scannerReticle:    { width: 240, height: 240 },
  scannerCorner:     { position: 'absolute', width: 28, height: 28, borderColor: C.accent, borderWidth: 3 },
  cornerTL:          { top: 0, left: 0, borderRightWidth: 0, borderBottomWidth: 0, borderTopLeftRadius: 4 },
  cornerTR:          { top: 0, right: 0, borderLeftWidth: 0, borderBottomWidth: 0, borderTopRightRadius: 4 },
  cornerBL:          { bottom: 0, left: 0, borderRightWidth: 0, borderTopWidth: 0, borderBottomLeftRadius: 4 },
  cornerBR:          { bottom: 0, right: 0, borderLeftWidth: 0, borderTopWidth: 0, borderBottomRightRadius: 4 },
  scannerCancel:     { backgroundColor: 'rgba(255,255,255,0.15)', borderRadius: 24, paddingVertical: 12, paddingHorizontal: 32 },
  scannerCancelText: { color: '#fff', fontSize: 16, fontWeight: '600', fontFamily: ARIMO },
  scannerError:      { color: C.red, fontSize: 16, textAlign: 'center', marginBottom: 24, fontFamily: ARIMO },

  // Chat layout
  safe:         { flex: 1, backgroundColor: C.bg },
  paneArea:     { flex: 1 },

  // Header
  header:       { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: 16, paddingVertical: 11, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  headerLeft:   { flexDirection: 'row', alignItems: 'center', gap: 8 },
  backBtn:      { paddingRight: 4, paddingVertical: 2 },
  backBtnText:  { fontSize: 32, lineHeight: 34, color: C.accent, fontWeight: '300', fontFamily: ARIMO },
  clearBtn:     { paddingVertical: 4, paddingHorizontal: 2 },
  clearBtnText: { fontSize: 14, color: C.textSecondary, fontWeight: '500', fontFamily: ARIMO },
  headerTitle:  { fontSize: 17, fontWeight: '700', color: C.textPrimary, letterSpacing: 1, fontFamily: ARIMO },
  connDot:      { width: 8, height: 8, borderRadius: 4 },

  // Chat pane
  pane:              { flex: 1, backgroundColor: C.bg },
  messageList:       { flex: 1 },
  messageListContent: { paddingVertical: 16 },
  emptyState:        { textAlign: 'center', color: C.textMuted, fontSize: 14, marginTop: 80, fontFamily: ARIMO },
  reconnectBanner:   { flexDirection: 'row', alignItems: 'center', justifyContent: 'center', paddingVertical: 7, backgroundColor: '#fffbeb', borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: '#fef3c7' },
  reconnectText:     { color: C.yellow, fontSize: 12, fontWeight: '500', fontFamily: ARIMO },

  // Scroll-to-bottom button
  scrollBtnWrap:     { position: 'absolute', left: 0, right: 0, alignItems: 'center', pointerEvents: 'box-none' },
  scrollBtn:         { backgroundColor: C.bg, borderRadius: 20, width: 36, height: 36, alignItems: 'center', justifyContent: 'center', shadowColor: '#000', shadowOpacity: 0.15, shadowRadius: 6, shadowOffset: { width: 0, height: 2 }, elevation: 4, borderWidth: StyleSheet.hairlineWidth, borderColor: C.border, marginBottom: 8 },
  scrollBtnIcon:     { fontSize: 18, color: C.textSecondary, lineHeight: 22, fontFamily: ARIMO },

  // Messages
  messageWrap:      { paddingHorizontal: 14, marginBottom: 14 },
  messageWrapRight: { alignItems: 'flex-end' },
  userBubble:       { backgroundColor: C.surface, borderRadius: 18, paddingHorizontal: 14, paddingVertical: 10, maxWidth: '80%' },
  textBlock:        { color: C.textPrimary, fontSize: 18, lineHeight: 26, fontWeight: '400', fontFamily: ARIMO },
  inlineCode:        { fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace', fontSize: 13, color: C.textPrimary, backgroundColor: C.surface, paddingHorizontal: 3, borderRadius: 3 },
  codeBlock:         { backgroundColor: C.surface, borderRadius: 6, padding: 10, marginVertical: 4 },
  codeBlockText:     { fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace', fontSize: 12, color: C.textPrimary, lineHeight: 18 },
  cursor:            { color: C.accent, fontSize: 14, fontFamily: ARIMO },
  questionMark:      { color: C.yellow, fontWeight: '700', fontSize: 15, marginBottom: 2, fontFamily: ARIMO },
  costLabel:         { fontSize: 11, color: C.textMuted, marginTop: 4, marginLeft: 2, fontFamily: ARIMO },
  toolLine:          { fontSize: 12, color: C.textMuted, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace', marginLeft: 2 },
  interruptedLine:   { fontSize: 18, lineHeight: 26, color: C.textMuted, fontFamily: ARIMO, fontStyle: 'italic' },

  // Input bar
  completionList: { maxHeight: 180, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: C.border },
  completionItem: { paddingHorizontal: 16, paddingVertical: 10, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  completionText: { fontSize: 14, color: C.textPrimary, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace' },
  inputFloat:   { position: 'absolute', bottom: 0, left: 0, right: 0, paddingHorizontal: 12, paddingBottom: 12 },
  input:        { backgroundColor: C.bg, borderWidth: 1, borderColor: C.inputBorder, borderRadius: 24, paddingHorizontal: 20, paddingVertical: 16, color: C.textPrimary, fontSize: 18, lineHeight: 26, minHeight: 48, maxHeight: 140, fontFamily: ARIMO, shadowColor: '#000', shadowOpacity: 0.08, shadowRadius: 12, shadowOffset: { width: 0, height: 2 }, elevation: 4 },
  inputStopBtn: { backgroundColor: C.bg, borderWidth: 1, borderColor: C.inputBorder, borderRadius: 24, paddingHorizontal: 20, paddingVertical: 16, height: 80, alignItems: 'center', justifyContent: 'center', shadowColor: '#000', shadowOpacity: 0.08, shadowRadius: 12, shadowOffset: { width: 0, height: 2 }, elevation: 4 },
  stopBtnText:  { fontSize: 14, color: C.red, fontWeight: '600', fontFamily: ARIMO },

  // Settings header button + dropdown
  headerRight:              { flexDirection: 'row', alignItems: 'center', gap: 8 },
  settingsMenuBtn:          { paddingVertical: 4, paddingHorizontal: 6 },
  settingsMenuBtnText:      { fontSize: 18, color: C.textSecondary, letterSpacing: 1, fontFamily: ARIMO },
  containerDot:             { width: 6, height: 6, borderRadius: 3 },
  containerMenuWrap:        { position: 'absolute', top: 44, right: 0, left: 0, bottom: 0, zIndex: 100 },
  containerMenu:            { position: 'absolute', right: 12, top: 4, minWidth: 240, maxHeight: '100%', backgroundColor: C.bg, borderRadius: 10, borderWidth: StyleSheet.hairlineWidth, borderColor: C.border, shadowColor: '#000', shadowOpacity: 0.12, shadowRadius: 12, shadowOffset: { width: 0, height: 4 }, elevation: 8, overflow: 'hidden' },
  containerMenuScroll:      { flexShrink: 1 },
  settingsMenuSection:      { paddingHorizontal: 14, paddingVertical: 8 },
  settingsMenuSectionTitle: { fontSize: 11, fontWeight: '700', color: C.textMuted, textTransform: 'uppercase', letterSpacing: 0.6, fontFamily: ARIMO },
  settingsMenuDivider:      { height: StyleSheet.hairlineWidth, backgroundColor: C.border },
  settingsMenuAction:       { paddingHorizontal: 14, paddingVertical: 13 },
  settingsMenuActionText:   { fontSize: 14, color: C.textSecondary, fontFamily: ARIMO },
  settingsMenuLogoutText:   { fontSize: 20, color: C.red, fontFamily: ARIMO },
  containerMenuItem:        { flexDirection: 'row', alignItems: 'center', gap: 10, paddingHorizontal: 14, paddingVertical: 12, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  containerMenuItemName:    { fontSize: 20, fontWeight: '600', color: C.textPrimary, fontFamily: ARIMO },
  containerMenuItemUrl:     { fontSize: 16, color: C.textMuted, fontFamily: ARIMO, marginTop: 1 },
  containerMenuItemStatus:  { fontSize: 16, color: C.textMuted, fontFamily: ARIMO },

})
