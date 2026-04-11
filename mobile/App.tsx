import AsyncStorage from '@react-native-async-storage/async-storage'
import React, { useCallback, useEffect, memo, useRef, useState } from 'react'
import {
  ActivityIndicator,
  Animated,
  AppState,
  FlatList,
  Keyboard,
  Modal,
  PermissionsAndroid,
  Platform,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  TouchableOpacity,
  useWindowDimensions,
  View,
} from 'react-native'
import { KeyboardProvider, useReanimatedKeyboardAnimation } from 'react-native-keyboard-controller'
import Reanimated, { useAnimatedStyle } from 'react-native-reanimated'
import { SafeAreaProvider, SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context'
import { Camera, useCameraDevice, useCodeScanner } from 'react-native-vision-camera'
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

type ServerFrame =
  | { type: 'history';          messages: HistMsg[]; live_gen: number }
  | { type: 'token';            text: string;        live_gen: number }
  | { type: 'tool';             name: string; input?: Record<string, unknown>; live_gen: number }
  | { type: 'question';         question: string;    live_gen: number }
  | { type: 'done';             cost_usd: number;    live_gen: number }
  | { type: 'error';            message: string;     live_gen: number }
  | { type: 'ack';              live_gen: number }
  | { type: 'session_start';    label: string; session_id: string; live_gen: number }
  | { type: 'session_end';      summary: string;     live_gen: number }
  | { type: 'container_list';        containers: ContainerInfo[] }
  | { type: 'container_status';      id: string; name: string; status: string }
  | { type: 'container_start_error'; id: string; message: string }

interface HistMsg { role: 'user' | 'assistant'; text: string }

interface Message {
  id:          string
  role:        'user' | 'assistant' | 'tool' | 'session'
  text:        string
  streaming?:  boolean
  isQuestion?: boolean
  cost?:       number
  label?:      string
  sessionId?:  string   // server-assigned UUID; set on session bubbles for reconnect matching
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
// bound port directly. (Hostnames like "localhost" won't work because the
// native Swift layer uses inet_pton which only accepts numeric IP addresses.)
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
  // Split on triple-backtick blocks first, then handle inline code within prose segments.
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
      // Within a prose block, split on inline backticks.
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
  if (query.includes(' ')) return null   // space ends the completion zone
  const lastSlash = query.lastIndexOf('/')
  return lastSlash === -1
    ? { atIndex, dirPart: '', filePart: query }
    : { atIndex, dirPart: query.slice(0, lastSlash + 1), filePart: query.slice(lastSlash + 1) }
}

// ── Container display name ─────────────────────────────────────────────────────

function containerDisplayName(name: string): string {
  return name.replace(/^claudulhu-/, '')
}

// ── Tool call formatting ───────────────────────────────────────────────────────

function formatToolCall(name: string, input?: Record<string, unknown>): string {
  const capName = name.split('_').map(w => w.charAt(0).toUpperCase() + w.slice(1)).join('')
  const entries = Object.entries(input ?? {})
  let args: string
  if (entries.length === 0) {
    args = ''
  } else if (entries.length === 1) {
    const val = String(entries[0][1])
    args = val.length > 120 ? val.slice(0, 120) + '…' : val
  } else {
    args = entries.map(([k, v]) => {
      const val = String(v)
      return `${k}=${val.length > 60 ? val.slice(0, 60) + '…' : val}`
    }).join(', ')
  }
  return `${capName}(${args})`
}

// ── AgentSessionBubble ────────────────────────────────────────────────────────

const AgentSessionBubble = memo(function AgentSessionBubble({ message }: { message: Message }) {
  const [expanded, setExpanded] = useState(false)
  const scrollRef    = useRef<ScrollView>(null)
  const pulseAnim    = useRef(new Animated.Value(0.5)).current
  const scanAnim     = useRef(new Animated.Value(0)).current
  const slideAnim    = useRef(new Animated.Value(1)).current
  const { height: screenHeight } = useWindowDimensions()
  const SHEET_HEIGHT = Math.round(screenHeight * 0.85)

  // Pulsing dot + scanning bar while streaming
  useEffect(() => {
    if (!message.streaming) {
      pulseAnim.setValue(1)
      scanAnim.setValue(0)
      return
    }
    const pulse = Animated.loop(Animated.sequence([
      Animated.timing(pulseAnim, { toValue: 1,    duration: 600, useNativeDriver: true }),
      Animated.timing(pulseAnim, { toValue: 0.25, duration: 600, useNativeDriver: true }),
    ]))
    const scan = Animated.loop(Animated.sequence([
      Animated.timing(scanAnim, { toValue: 1, duration: 1200, useNativeDriver: true }),
      Animated.timing(scanAnim, { toValue: 0, duration: 0,    useNativeDriver: true }),
    ]))
    pulse.start()
    scan.start()
    return () => { pulse.stop(); scan.stop() }
  }, [message.streaming])

  const openSheet = useCallback(() => {
    slideAnim.setValue(1)
    setExpanded(true)
    Animated.spring(slideAnim, { toValue: 0, useNativeDriver: true, tension: 70, friction: 12 }).start()
  }, [slideAnim])

  const closeSheet = useCallback(() => {
    Animated.timing(slideAnim, { toValue: 1, duration: 240, useNativeDriver: true }).start(() => setExpanded(false))
  }, [slideAnim])

  const sheetTranslate = slideAnim.interpolate({
    inputRange: [0, 1],
    outputRange: [0, SHEET_HEIGHT],
  })

  // Show last portion of log so the preview tail tracks live output
  const preview = message.text.length > 400
    ? '\u2026' + message.text.slice(-400)
    : message.text

  return (
    <View style={s.sessionWrap}>
      <TouchableOpacity onPress={openSheet} activeOpacity={0.85}>
        <View style={s.sessionBox}>
          {/* Header bar */}
          <View style={s.sessionBoxHeader}>
            <Animated.View style={[s.sessionDot, {
              opacity: pulseAnim,
              backgroundColor: message.streaming ? C.accent : C.green,
            }]} />
            <Text style={s.sessionBoxLabel}>
              {message.streaming ? (message.label ?? 'working\u2026') : 'session log'}
            </Text>
            <Text style={s.sessionExpandHint}>&#x2197;</Text>
          </View>

          {/* Animated scan line under header when streaming */}
          {message.streaming && (
            <Animated.View style={[s.sessionScanLine, {
              transform: [{ translateX: scanAnim.interpolate({
                inputRange: [0, 1], outputRange: [-300, 300],
              }) }],
            }]} />
          )}

          {/* Preview text — last ~3 lines of session output */}
          <Text style={s.sessionPreviewText} numberOfLines={3}>{preview}</Text>
        </View>
      </TouchableOpacity>

      {/* Expanded bottom-sheet log */}
      <Modal visible={expanded} transparent animationType="none" onRequestClose={closeSheet}>
        <TouchableOpacity style={s.sessionSheetBackdrop} activeOpacity={1} onPress={closeSheet} />
        <Animated.View style={[s.sessionSheet, { height: SHEET_HEIGHT, transform: [{ translateY: sheetTranslate }] }]}>
          <View style={s.sessionSheetHandle} />
          <View style={s.sessionModalHeader}>
            <View style={s.sessionBoxHeader}>
              <View style={[s.sessionDot, { backgroundColor: message.streaming ? C.accent : C.green }]} />
              <Text style={s.sessionModalTitle}>
                {message.streaming ? (message.label ?? 'working\u2026') : 'session log'}
              </Text>
            </View>
            <TouchableOpacity onPress={closeSheet} hitSlop={{ top: 8, bottom: 8, left: 16, right: 8 }}>
              <Text style={s.sessionModalClose}>Done</Text>
            </TouchableOpacity>
          </View>
          <ScrollView
            ref={scrollRef}
            style={s.sessionModalScroll}
            onContentSizeChange={() => scrollRef.current?.scrollToEnd({ animated: false })}
          >
            <Text style={s.sessionModalText}>{message.text}</Text>
          </ScrollView>
        </Animated.View>
      </Modal>
    </View>
  )
})

// ── MessageBubble ─────────────────────────────────────────────────────────────

const MessageBubble = memo(function MessageBubble({ message }: { message: Message }) {
  if (message.role === 'session') {
    return <AgentSessionBubble message={message} />
  }
  if (message.role === 'tool') {
    return (
      <View style={s.messageWrap}>
        <Text style={s.toolLine}>{message.text}</Text>
      </View>
    )
  }
  if (message.role === 'user') {
    return (
      <View style={[s.messageWrap, s.messageWrapRight]}>
        <View style={s.userBubble}>
          {renderText(message.text, s.textBlock)}
          {message.streaming && <Text style={s.cursor}>▋</Text>}
        </View>
      </View>
    )
  }
  return (
    <View style={s.messageWrap}>
      {message.isQuestion && <Text style={s.questionMark}>?</Text>}
      {renderText(message.text, s.textBlock)}
      {message.streaming && <Text style={s.cursor}>▋</Text>}
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
  wsUrl, connKey, onStatusChange, clearRef, interruptRef, onContainerFrame, sendRef,
}: {
  wsUrl:              string
  connKey:            string
  onStatusChange:     (s: ConnStatus) => void
  clearRef:           React.MutableRefObject<() => void>
  interruptRef:       React.MutableRefObject<() => void>
  onContainerFrame?:  (frame: ServerFrame) => void
  sendRef?:           React.MutableRefObject<(msg: object) => void>
}) {
  const insets                     = useSafeAreaInsets()
  const { height: keyboardHeight } = useReanimatedKeyboardAnimation()
  const spacerStyle                = useAnimatedStyle(() => ({
    height: Math.max(insets.bottom, -keyboardHeight.value),
  }))

  const [messages,       setMessagesState]  = useState<Message[]>([])
  const messagesRef = useRef<Message[]>([])
  // Keep messagesRef in sync immediately (not via useEffect) so that
  // ws.onmessage handlers always read the current list, even when React
  // hasn't re-rendered yet (e.g. the 'history' frame arrives before the
  // useEffect that would sync the ref from the AsyncStorage load).
  const setMessages = useCallback((arg: Message[] | ((prev: Message[]) => Message[])) => {
    setMessagesState(prev => {
      const next = typeof arg === 'function' ? arg(prev) : arg
      messagesRef.current = next
      return next
    })
  }, [])
  const [status,         setStatus]         = useState<ConnStatus>('connecting')
  const [input,          setInput]          = useState('')
  const [pendingQuestion, setPendingQuestion] = useState(false)
  const [completions,    setCompletions]    = useState<string[]>([])
  const [showScrollBtn,  setShowScrollBtn]  = useState(false)
  const [inputAreaH,     setInputAreaH]     = useState(0)

  const httpBase = wsUrl.replace(/^ws:/, 'http:').replace(/\/chat$/, '')

  const wsRef           = useRef<WebSocket | null>(null)
  const sendMessageRef  = useRef<() => void>(() => {})
  const listRef         = useRef<FlatList<Message>>(null)
  const isAtBottomRef   = useRef(true)
  // Text of the last sent message that hasn't been ack'd by the server yet.
  // Persisted to AsyncStorage so it survives a killed connection; cleared on
  // ack or when confirmed present in the next history frame.
  const pendingMsgRef   = useRef<string | null>(null)

  // Whether we have already loaded the cached message list from AsyncStorage.
  // Subsequent wsUrl changes (tunnel port changes on reconnect) must NOT
  // overwrite in-memory messages with the stale persisted snapshot.
  const storageLoadedRef = useRef(false)

  // Incremented every time connect() is called.  Each ws.onmessage closure
  // captures the epoch at creation time and discards frames if the epoch no
  // longer matches — this prevents frames from a previous (stale) socket
  // leaking into the new connection even when live_gen happens to be the same.
  const connEpochRef    = useRef<number>(0)

  // The live_gen value from the most-recent 'history' frame.  Any live frame
  // (token/tool/question/done/error) whose live_gen differs is stale and must
  // be discarded — it belongs to a prior connection's replay that raced with
  // the history frame of the current connection.
  const liveGenRef      = useRef<number>(-1)

  // Accumulates token text for responses that never call session_start (simple
  // answers). Used as the assistant message text on 'done'.
  const pendingTextRef  = useRef<string>('')

  // ID of the session bubble currently being streamed into.  Set on
  // 'session_start', cleared on 'session_end' / 'done' / 'error'.
  const currentSessionIdRef = useRef<string | null>(null)


  // Fetch @ completions whenever the input changes.
  useEffect(() => {
    const parsed = parseAtQuery(input)
    if (!parsed) { setCompletions([]); return }
    let cancelled = false
    fetch(`${httpBase}/completions?dir_part=${encodeURIComponent(parsed.dirPart)}&file_part=${encodeURIComponent(parsed.filePart)}`)
      .then(r => r.json())
      .then((data: string[]) => { if (!cancelled) setCompletions(data) })
      .catch(() => { if (!cancelled) setCompletions([]) })
    return () => { cancelled = true }
  }, [input, httpBase])

  const applyCompletion = useCallback((completion: string) => {
    const parsed = parseAtQuery(input)
    if (!parsed) return
    const newText = input.slice(0, parsed.atIndex + 1) + completion
    if (completion.endsWith('/')) {
      setInput(newText)
      // useEffect will re-fetch the next directory level
    } else {
      setInput(newText + ' ')
      setCompletions([])
    }
  }, [input])

  const updateStatus = useCallback((s: ConnStatus) => {
    setStatus(s)
    onStatusChange(s)
  }, [onStatusChange])

  // WebSocket connection lifecycle
  useEffect(() => {
    let cancelled = false
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null

    // Load both cached messages and the unacknowledged-pending entry before
    // connecting.  If we connected first there would be a race where the
    // 'history' frame arrives before pendingMsgRef is restored, causing the
    // message to be silently dropped instead of resent.
    //
    // Only load on the very first connection for this chat pane.  On subsequent
    // wsUrl changes (tunnel port changes on reconnect) the in-memory messages
    // are already up-to-date; overwriting them with the stale persisted snapshot
    // would cause the session log to disappear.
    const connect = () => {
      if (cancelled) return
      // Close any existing socket synchronously so its in-flight frames cannot
      // race with the new connection's frames (they would share the same
      // live_gen and bypass the stale-frame guard).
      wsRef.current?.close()
      updateStatus('connecting')
      // Clear stale session ID so the token/tool fallback path can create a
      // fresh session bubble if the server is mid-stream on reconnect.
      currentSessionIdRef.current = null
      // Advance the epoch so any onmessage closures from old sockets discard
      // their frames immediately.
      const myEpoch = ++connEpochRef.current
      const ws = new WebSocket(wsUrl)
      wsRef.current = ws

      ws.onopen = () => {}

      ws.onmessage = ({ data }: { data: string }) => {
        if (cancelled || connEpochRef.current !== myEpoch) return
        let frame: ServerFrame
        try { frame = JSON.parse(data as string) } catch { return }


        // Route master-specific frames to AppInner without touching chat state.
        if (frame.type === 'container_list' || frame.type === 'container_status' || frame.type === 'container_start_error') {
          onContainerFrame?.(frame)
          return
        }

        switch (frame.type) {
          case 'history': {
            // Record the authoritative live_gen from this snapshot so we can
            // discard any stale live frames that arrive after reconnect.
            liveGenRef.current = frame.live_gen
            // Clear stale streaming state from the previous connection so that
            // tokens from the new stream don't append to a partial pendingTextRef
            // accumulated before the disconnect, which caused message duplication.
            pendingTextRef.current = ''
            currentSessionIdRef.current = null

            const serverMsgs: Message[] = frame.messages.map((m, i) => ({
              id: `h${i}`, role: m.role, text: m.text,
            }))

            // Re-insert client-only bubbles (session, tool, and assistant
            // summaries produced by session_end) from the local cache so they
            // survive reconnection.
            //
            // Background: the server's history only contains messages that have
            // plain-text content blocks.  Agentic-session assistant turns
            // (which consist entirely of tool-use blocks) produce NO entry in
            // the server history.  The session bubble itself, any tool lines
            // inside it, and the assistant summary emitted by session_end are
            // therefore CLIENT-ONLY — they will never appear in base.
            //
            // Strategy: walk both local and base in parallel, matching
            // user/assistant server messages by text.  Between each pair of
            // matched server entries, splice back in whatever client-only
            // bubbles sat there in local.  After base is exhausted, flush ALL
            // remaining local entries (not just session/tool) so that trailing
            // assistant summaries are never silently dropped.
            const mergeSessionBubbles = (base: Message[]): Message[] => {
              const local = messagesRef.current
              if (!local.length) return base

              const result: Message[] = []
              let li = 0 // pointer into local

              for (let bi = 0; bi < base.length; bi++) {
                const bm = base[bi]

                // Collect any client-only bubbles that precede the next
                // server-matched anchor in local.
                //
                // A message is considered client-only (will never appear in
                // the server history frame) if:
                //   • Its role is 'session' or 'tool' (always client-only), OR
                //   • Its role is 'assistant' AND we have already seen a
                //     'session' bubble in the current pending run — such an
                //     assistant message is the session_end summary, which the
                //     server never includes in history because the agentic
                //     assistant turn has only tool-use blocks (no text).
                //
                // Regular (non-agentic) assistant replies DO appear in server
                // history and must NOT be swallowed here.
                const pending: Message[] = []
                let sawSession = false
                while (li < local.length) {
                  const lm = local[li]
                  if (lm.role === 'session' || lm.role === 'tool') {
                    sawSession = true
                    pending.push(lm)
                    li++
                  } else if (lm.role === 'assistant' && sawSession) {
                    // session_end summary following a session bubble — client-only
                    pending.push(lm)
                    li++
                  } else {
                    break
                  }
                }

                // Try to match bm against the next local server message.
                if (li < local.length && local[li].role === bm.role && local[li].text === bm.text) {
                  // Match — splice in the buffered client-only bubbles first,
                  // then the server message (preserving client-only fields and
                  // the local id so FlatList keys are stable across reconnects).
                  result.push(...pending.map(m => ({ ...m, streaming: false })))
                  const lm = local[li]
                  result.push({ ...bm, id: lm.id, ...(lm.cost != null ? { cost: lm.cost } : {}) })
                  li++
                } else {
                  // No match (local is stale/different) — still emit any
                  // buffered client-only bubbles so they aren't lost, then
                  // emit the server message with a fresh unique ID.
                  // Using bm's h${i} id here would collide with a later
                  // iteration that matches a local message whose id is also
                  // h${i}, causing FlatList key conflicts and visible duplication.
                  result.push(...pending.map(m => ({ ...m, streaming: false })))
                  result.push({ ...bm, id: uid() })
                  // Do NOT advance li: we haven't matched local[li] yet, so
                  // leave the pointer in place for the next iteration.
                }
              }

              // After base is exhausted, flush ALL remaining local entries.
              // This covers:
              //  • A session bubble (and its tool lines) that was still live at
              //    disconnect time.  We mark it streaming:false here because
              //    session_start will create a fresh bubble for the server replay;
              //    keeping the old one streaming would leave two live bubbles.
              //  • A completed session bubble followed by the assistant summary
              //    produced by session_end — the summary has role='assistant'
              //    and is client-only (never in server history), so the old
              //    "session/tool only" flush silently dropped it every reconnect.
              while (li < local.length) {
                const lm = local[li++]
                result.push(lm.streaming ? { ...lm, streaming: false } : lm)
              }

              return result
            }

            // If there's an unacknowledged pending message check whether the
            // server already has it in history.
            const pending = pendingMsgRef.current
            let didResend = false
            if (pending) {
              const lastUserMsg = [...frame.messages].reverse().find(m => m.role === 'user')
              if (lastUserMsg?.text === pending) {
                // Server has it — clear the pending entry.
                pendingMsgRef.current = null
                AsyncStorage.removeItem(`pending_${connKey}`).catch(() => {})
                // Server has the message; use authoritative history as-is,
                // but re-insert any local session/tool bubbles.
                setMessages(mergeSessionBubbles(serverMsgs))
              } else {
                // Server never received it — show optimistic user bubble then
                // resend.  The ack will clear pendingMsgRef.
                pendingMsgRef.current = null
                AsyncStorage.removeItem(`pending_${connKey}`).catch(() => {})
                const optimisticBubble: Message = { id: uid(), role: 'user', text: pending }
                pendingTextRef.current = ''
                currentSessionIdRef.current = null
                setMessages([...mergeSessionBubbles(serverMsgs), optimisticBubble])
                ws.send(JSON.stringify({ type: 'message', text: pending }))
                updateStatus('streaming')
                didResend = true
              }
            } else {
              // No pending message — server is the ground truth, but
              // re-insert local session/tool bubbles so they survive reconnect.
              setMessages(mergeSessionBubbles(serverMsgs))
            }

            setPendingQuestion(false)
            // Only force-scroll on initial load (list was empty) or if user
            // was already at the bottom.  Don't clobber isAtBottomRef when
            // the user is scrolled up mid-stream — that's what stopped the
            // scroll-to-bottom button from ever appearing.
            if (isAtBottomRef.current || serverMsgs.length === 0) {
              setTimeout(() => listRef.current?.scrollToEnd({ animated: false }), 50)
            }
            if (!didResend) {
              updateStatus('ready')
            }
            // Persist the merged list (including session bubbles) so the next
            // reconnect also has them available.
            AsyncStorage.setItem(`msgs_${connKey}`, JSON.stringify(messagesRef.current)).catch(() => {})
            break
          }
          case 'session_start': {
            if (frame.live_gen !== liveGenRef.current) break
            updateStatus('streaming')
            pendingTextRef.current = ''
            // Create a fresh bubble for this session.  On reconnect, the
            // history-merge step may have already re-inserted the previous
            // (incomplete) session bubble from local cache.  Remove any such
            // stale bubble (matched by sessionId) before appending the new one
            // so we don't end up with two session bubbles for the same session.
            const sid = uid()
            currentSessionIdRef.current = sid
            setMessages(prev => [
              ...prev.filter(m => m.sessionId !== frame.session_id),
              { id: sid, role: 'session' as const, text: '', streaming: true, label: frame.label, sessionId: frame.session_id },
            ])
            break
          }
          case 'session_end': {
            if (frame.live_gen !== liveGenRef.current) break
            const finishedSessionId = currentSessionIdRef.current
            currentSessionIdRef.current = null
            pendingTextRef.current = ''
            setMessages(prev => {
              const finalized = prev.map(m =>
                (finishedSessionId ? m.id === finishedSessionId : m.streaming && m.role === 'session')
                  ? { ...m, streaming: false }
                  : m
              )
              // Only append the summary bubble if there is actually summary text.
              // Sessions that only call tools with no final text response should
              // not create an empty assistant bubble.
              return frame.summary
                ? [...finalized, { id: uid(), role: 'assistant' as const, text: frame.summary }]
                : finalized
            })
            break
          }
          case 'token': {
            // Discard tokens from a stale generation (old connection replay).
            if (frame.live_gen !== liveGenRef.current) break
            updateStatus('streaming')
            // Always accumulate in pendingTextRef. The session bubble shows only
            // tool call lines (added via 'tool' events). The final response text
            // is appended as a clean assistant bubble by session_end. Streaming
            // tokens into the session bubble caused the response to appear twice.
            pendingTextRef.current += frame.text
            break
          }
          case 'done': {
            // Discard done from a stale generation.
            if (frame.live_gen !== liveGenRef.current) break
            const cost = frame.cost_usd
            const pendingText = pendingTextRef.current
            pendingTextRef.current = ''
            currentSessionIdRef.current = null
            setMessages(prev => {
              // If session_end already ran, just attach cost to the last assistant message.
              // If this is a simple answer (no session), append the accumulated text now.
              const last = prev[prev.length - 1]
              const finalized = prev.map(m => m.streaming ? { ...m, streaming: false } : m)
              if (pendingText) {
                // Simple answer path — no session bubble was ever opened.
                const summaryMsg: Message = { id: uid(), role: 'assistant' as const, text: pendingText, cost }
                const updated = [...finalized, summaryMsg]
                AsyncStorage.setItem(`msgs_${connKey}`, JSON.stringify(updated)).catch(() => {})
                return updated
              } else {
                // Session path — session_end already appended the summary; attach cost to it.
                const updated = finalized.map((m, i) =>
                  i === finalized.length - 1 && m.role === 'assistant' ? { ...m, cost } : m
                )
                AsyncStorage.setItem(`msgs_${connKey}`, JSON.stringify(updated)).catch(() => {})
                return updated
              }
            })
            updateStatus('ready')
            setPendingQuestion(false)
            break
          }
          case 'question': {
            // Discard questions from a stale generation.
            if (frame.live_gen !== liveGenRef.current) break
            pendingTextRef.current = ''
            currentSessionIdRef.current = null
            setMessages(prev => {
              const finalized = prev.map(m => m.streaming ? { ...m, streaming: false } : m)
              return [...finalized, { id: uid(), role: 'assistant' as const, text: frame.question, isQuestion: true }]
            })
            setPendingQuestion(true)
            updateStatus('ready')
            break
          }
          case 'tool': {
            // Discard tool frames from a stale generation.
            if (frame.live_gen !== liveGenRef.current) break
            const toolLine = '\n\u25b8 ' + formatToolCall(frame.name, frame.input) + '\n'
            const sid = currentSessionIdRef.current
            if (sid) {
              setMessages(prev => prev.map(m => m.id === sid ? { ...m, text: m.text + toolLine, streaming: true } : m))
            }
            break
          }
          case 'ack': {
            // Update liveGenRef to the gen the server is about to stream.
            // Without this, all live frames would be discarded as "stale"
            // because they carry a gen one higher than the history frame.
            liveGenRef.current = frame.live_gen
            pendingMsgRef.current = null
            AsyncStorage.removeItem(`pending_${connKey}`).catch(() => {})
            break
          }
          case 'error': {
            // Discard errors from a stale generation.
            if (frame.live_gen !== liveGenRef.current) break
            pendingTextRef.current = ''
            currentSessionIdRef.current = null
            setMessages(prev => {
              const finalized = prev.map(m => m.streaming ? { ...m, streaming: false } : m)
              const updated = [...finalized, { id: uid(), role: 'assistant' as const, text: `\u2717 ${frame.message}` }]
              AsyncStorage.setItem(`msgs_${connKey}`, JSON.stringify(updated)).catch(() => {})
              return updated
            })
            updateStatus('ready')
            break
          }
        }
      }

      ws.onerror = (e: Event) => {
        if (cancelled) return
        updateStatus('error')
      }

      ws.onclose = (e: CloseEvent) => {
        if (cancelled) return
        updateStatus('connecting')
        reconnectTimer = setTimeout(connect, 1500)
      }
    }

    if (!storageLoadedRef.current) {
      Promise.all([
        AsyncStorage.getItem(`msgs_${connKey}`).catch(() => null),
        AsyncStorage.getItem(`pending_${connKey}`).catch(() => null),
      ]).then(([msgsJson, pendingText]) => {
        if (cancelled) return
        storageLoadedRef.current = true
        if (msgsJson) try { setMessages(JSON.parse(msgsJson)) } catch {}
        if (pendingText) pendingMsgRef.current = pendingText
        connect()
      })
    } else {
      connect()
    }

    return () => {
      cancelled = true
      if (reconnectTimer) clearTimeout(reconnectTimer)
      wsRef.current?.close()
    }
  }, [wsUrl, connKey])

  // When app foregrounds: force-close WS so onclose fires and we reconnect.
  // (Noise tunnel re-establishment is handled by AppInner for the master
  // connection, and by ChildChatScreen for child connections.)
  useEffect(() => {
    const sub = AppState.addEventListener('change', nextState => {
      if (nextState === 'active') {
        const ws = wsRef.current
        if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) {
          ws.close()
        }
      }
    })
    return () => sub.remove()
  }, [])

  // Scroll to bottom when the keyboard appears, but only if already near bottom.
  useEffect(() => {
    const sub = Keyboard.addListener('keyboardDidShow', () => {
      if (isAtBottomRef.current) {
        listRef.current?.scrollToEnd({ animated: false })
      }
    })
    return () => sub.remove()
  }, [])

  const sendMessage = useCallback(() => {
    const text = input.trim()
    if (!text || status === 'streaming') return
    const ws = wsRef.current
    if (!ws || ws.readyState !== WebSocket.OPEN) return

    // Reset session state for this new turn.
    currentSessionIdRef.current = null
    pendingTextRef.current = ''
    setMessages(prev => [
      ...prev,
      { id: uid(), role: 'user' as const, text },
    ])
    isAtBottomRef.current = true

    if (pendingQuestion) {
      ws.send(JSON.stringify({ type: 'answer', answer: text }))
      setPendingQuestion(false)
    } else {
      // Persist before sending so a dropped connection doesn't silently lose
      // the message.  Cleared on ack from server or confirmed in next history.
      pendingMsgRef.current = text
      AsyncStorage.setItem(`pending_${connKey}`, text).catch(() => {})
      ws.send(JSON.stringify({ type: 'message', text }))
    }
    updateStatus('streaming')

    setInput('')
  }, [input, pendingQuestion, status])

  sendMessageRef.current = sendMessage

  const clearConversation = useCallback(() => {
    // Invalidate the current live generation immediately so any in-flight
    // streaming frames that arrive before the server's history response are
    // discarded, not appended to the now-empty conversation.
    liveGenRef.current = -1
    currentSessionIdRef.current = null
    pendingTextRef.current = ''
    wsRef.current?.send(JSON.stringify({ type: 'clear' }))
    setMessages([])
    setPendingQuestion(false)
    AsyncStorage.removeItem(`msgs_${connKey}`).catch(() => {})
  }, [connKey])
  clearRef.current = clearConversation

  interruptRef.current = () => {
    wsRef.current?.send(JSON.stringify({ type: 'interrupt' }))
  }

  if (sendRef) {
    sendRef.current = (msg: object) => {
      wsRef.current?.send(JSON.stringify(msg))
    }
  }

  const isPending = status === 'streaming' && messages.length > 0 && !messages[messages.length - 1]?.streaming

  return (
    <View style={s.pane}>
      <View style={{ flex: 1 }}>
        <FlatList
          ref={listRef}
          data={messages}
          keyExtractor={m => m.id}
          renderItem={({ item }) => <MessageBubble message={item} />}
          contentContainerStyle={[s.messageListContent, { paddingBottom: 8 }]}
          style={s.messageList}
          ListEmptyComponent={<Text style={s.emptyState}>say something</Text>}
          onContentSizeChange={() => {
            if (isAtBottomRef.current) {
              listRef.current?.scrollToEnd({ animated: false })
            }
          }}
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

        {(status === 'connecting' || status === 'error') && (
          <View style={s.reconnectBanner}>
            {status !== 'error' && <ActivityIndicator size="small" color={C.yellow} style={{ marginRight: 6 }} />}
            <Text style={s.reconnectText}>
              {status === 'error' ? 'connection error — retrying…' : 'reconnecting…'}
            </Text>
          </View>
        )}

        <View style={{ backgroundColor: C.surface }} onLayout={e => setInputAreaH(e.nativeEvent.layout.height)}>
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
          <View style={s.inputRow}>
            <TextInput
              style={s.input}
              value={input}
              onChangeText={text => {
                if (text.includes('\n')) { sendMessageRef.current(); return }
                setInput(text)
              }}
              onSubmitEditing={() => sendMessageRef.current()}
              placeholder={pendingQuestion ? 'answer…' : 'message…'}
              placeholderTextColor={C.textMuted}
              multiline
              returnKeyType="send"
              blurOnSubmit={false}
              editable={status !== 'streaming'}
            />
          </View>
        </View>

        {showScrollBtn && (
          <View style={[s.scrollBtnWrap, { bottom: inputAreaH }]} pointerEvents="box-none">
            <TouchableOpacity
              style={s.scrollBtn}
              onPress={() => {
                isAtBottomRef.current = true
                setShowScrollBtn(false)
                listRef.current?.scrollToEnd({ animated: true })
              }}
              activeOpacity={0.75}
            >
              <Text style={s.scrollBtnIcon}>↓</Text>
            </TouchableOpacity>
          </View>
        )}
      </View>
      {/* Spacer whose height matches the keyboard height (or bottom safe area when
          keyboard is hidden). Growing this spacer shrinks the flex:1 content above,
          so the entire conversation+input block moves up with the keyboard. */}
      <Reanimated.View style={[{ backgroundColor: C.surface }, spacerStyle]} />
    </View>
  )
})


// ── ChildChatScreen ───────────────────────────────────────────────────────────

function ChildChatScreen({ child, onClose }: {
  child:   ContainerInfo
  onClose: () => void
}) {
  const [childTunnelPort, setChildTunnelPort] = useState<number | null>(null)
  const [tunnelError,     setTunnelError]     = useState<string | null>(null)
  const [chatStatus,      setChatStatus]      = useState<ConnStatus>('connecting')
  const clearRef     = useRef<() => void>(() => {})
  const interruptRef = useRef<() => void>(() => {})

  useEffect(() => {
    let cancelled = false
    if (!NoiseConnection) {
      setTunnelError('Native Noise module unavailable')
      return
    }
    NoiseConnection!.disconnect()
    NoiseConnection!.connect(child.host, child.port, child.pubkey)
      .then(port => { if (!cancelled) setChildTunnelPort(port) })
      .catch(e => { if (!cancelled) setTunnelError(e?.message ?? String(e)) })
    return () => {
      cancelled = true
      NoiseConnection?.disconnect()
    }
  }, [])

  // Re-establish child Noise tunnel when app returns to foreground (the native
  // TCP proxy is killed during suspension, so the WS reconnect would otherwise
  // silently fail — same logic as AppInner does for the master tunnel).
  useEffect(() => {
    const sub = AppState.addEventListener('change', nextState => {
      if (nextState === 'active') {
        NoiseConnection?.disconnect()
        NoiseConnection?.connect(child.host, child.port, child.pubkey)
          .then(port => setChildTunnelPort(port))
          .catch(e => setTunnelError(e?.message ?? String(e)))
      }
    })
    return () => sub.remove()
  }, [child.host, child.port, child.pubkey])

  const handleBack = useCallback(() => {
    onClose()
  }, [onClose])

  const connKey = `child:${child.id}`

  return (
    <SafeAreaView style={s.safe} edges={['top']}>
      <View style={s.paneArea}>
        <View style={s.header}>
          <View style={s.headerLeft}>
            <TouchableOpacity
              style={s.backBtn}
              onPress={handleBack}
              hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
            >
              <Text style={s.backBtnText}>‹</Text>
            </TouchableOpacity>
            <View style={[s.connDot, { backgroundColor: statusColor(chatStatus) }]} />
            <View>
              <Text style={s.headerTitle}>{containerDisplayName(child.name)}</Text>
            </View>
          </View>
          {chatStatus === 'streaming' ? (
            <TouchableOpacity
              style={s.clearBtn}
              onPress={() => interruptRef.current()}
              hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
            >
              <Text style={s.stopBtnText}>■ stop</Text>
            </TouchableOpacity>
          ) : (
            <TouchableOpacity
              style={s.clearBtn}
              onPress={() => clearRef.current()}
              hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
              disabled={chatStatus !== 'ready'}
            >
              <Text style={[s.clearBtnText, chatStatus !== 'ready' && { opacity: 0.3 }]}>clear</Text>
            </TouchableOpacity>
          )}
        </View>

        {childTunnelPort ? (
          <ChatPane
            wsUrl={`ws://127.0.0.1:${childTunnelPort}/chat`}
            connKey={connKey}
            onStatusChange={setChatStatus}
            clearRef={clearRef}
            interruptRef={interruptRef}
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
  const [containers,           setContainers]           = useState<ContainerInfo[]>([])
  const [activeChild,          setActiveChild]          = useState<ContainerInfo | null>(null)
  const [showSettingsMenu,     setShowSettingsMenu]     = useState(false)
  const [startingContainerId,  setStartingContainerId]  = useState<string | null>(null)
  const [startingError,        setStartingError]        = useState<string | null>(null)
  const startingContainerIdRef = useRef<string | null>(null)
  const masterSendRef          = useRef<(msg: object) => void>(() => {})
  // Incrementing this forces the master Noise tunnel effect to re-run and
  // re-establish the master connection after a child screen closes.
  const [noiseKey,    setNoiseKey]    = useState(0)
  const clearChatRef     = useRef<() => void>(() => {})
  const interruptChatRef = useRef<() => void>(() => {})

  // Load saved master connection on mount and auto-connect.
  useEffect(() => {
    let cancelled = false
    const load = async () => {
      let saved: NoiseConnectionInfo | null = null
      if (__DEV__) {
        saved = DEV_CONN
      } else {
        const json = await AsyncStorage.getItem('masterConnection').catch(() => null)
        if (json) { try { saved = JSON.parse(json) } catch {} }
      }
      if (!cancelled && saved) setConn(saved)
    }
    load()
    return () => { cancelled = true }
  }, [])

  // Establish Noise tunnel when conn changes or after a child modal closes
  // (noiseKey is incremented on child close to force reconnection to master).
  useEffect(() => {
    setTunnelPort(null)
    setTunnelError(null)
    if (!conn) return
    if (!NoiseConnection) {
      setTunnelError('Native Noise module unavailable')
      return
    }
    let connected = false
    const timer = setTimeout(() => {
      NoiseConnection!.connect(conn.host, conn.port, conn.pk)
        .then(port => { connected = true; setTunnelPort(port) })
        .catch(e => setTunnelError(e?.message ?? String(e)))
    }, 50)
    return () => {
      clearTimeout(timer)
      if (connected) NoiseConnection?.disconnect()
    }
  }, [conn, noiseKey])

  // Re-establish Noise tunnel when app returns to foreground (iOS kills the
  // native TCP proxy during suspension; without this the WS reconnect silently fails).
  useEffect(() => {
    if (!conn) return
    const sub = AppState.addEventListener('change', nextState => {
      if (nextState === 'active') {
        NoiseConnection?.disconnect()
        NoiseConnection?.connect(conn.host, conn.port, conn.pk)
          .then(port => setTunnelPort(port))
          .catch(e => setTunnelError(e?.message ?? String(e)))
      }
    })
    return () => sub.remove()
  }, [conn])

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

  const handleContainerFrame = useCallback((frame: ServerFrame) => {
    if (frame.type === 'container_list') {
      setContainers(frame.containers)
      const waitingId = startingContainerIdRef.current
      if (waitingId) {
        const started = frame.containers.find(c => c.id === waitingId && c.status === 'running' && c.pubkey)
        if (started) {
          startingContainerIdRef.current = null
          setStartingContainerId(null)
          setStartingError(null)
          setActiveChild(started)
        }
      }
    } else if (frame.type === 'container_status') {
      setContainers(prev => prev.map(c =>
        c.id === frame.id ? { ...c, status: frame.status } : c
      ))
    } else if (frame.type === 'container_start_error') {
      if (frame.id === startingContainerIdRef.current) {
        setStartingError(frame.message)
      }
    }
  }, [])

  // ── QR scanner overlay ──────────────────────────────────────────────────────
  if (scanning) {
    return <QrScanner onScanned={handleQrScanned} onCancel={() => setScanning(false)} />
  }

  // ── Connecting screen (conn selected, tunnel not yet up) ────────────────────
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
    return (
      <ChildChatScreen
        child={activeChild}
        onClose={() => {
          setActiveChild(null)
          setNoiseKey(k => k + 1)
        }}
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
            {chatStatus === 'streaming' ? (
              <TouchableOpacity
                style={s.clearBtn}
                onPress={() => interruptChatRef.current()}
                hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
              >
                <Text style={s.stopBtnText}>■ stop</Text>
              </TouchableOpacity>
            ) : (
              <TouchableOpacity
                style={s.clearBtn}
                onPress={() => clearChatRef.current()}
                hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
                disabled={chatStatus !== 'ready'}
              >
                <Text style={[s.clearBtnText, chatStatus !== 'ready' && { opacity: 0.3 }]}>clear</Text>
              </TouchableOpacity>
            )}
            <TouchableOpacity
              style={s.settingsMenuBtn}
              onPress={() => setShowSettingsMenu(v => !v)}
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
                      startingContainerIdRef.current = c.id
                      setStartingContainerId(c.id)
                      setStartingError(null)
                      masterSendRef.current({ type: 'start_container', id: c.id })
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
              <View style={s.settingsMenuDivider} />
              <TouchableOpacity style={s.settingsMenuAction} onPress={handleLogout}>
                <Text style={s.settingsMenuLogoutText}>logout</Text>
              </TouchableOpacity>
            </View>
          </View>
        )}

        <ChatPane
          wsUrl={`ws://127.0.0.1:${tunnelPort}/chat`}
          connKey={connKeyFor(conn)}
          onStatusChange={setChatStatus}
          clearRef={clearChatRef}
          interruptRef={interruptChatRef}
          onContainerFrame={handleContainerFrame}
          sendRef={masterSendRef}
        />

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
  textBlock:        { color: C.textPrimary, fontSize: 18, lineHeight: 27, fontWeight: '400', fontFamily: ARIMO },
  inlineCode:        { fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace', fontSize: 13, color: C.textPrimary, backgroundColor: C.surface, paddingHorizontal: 3, borderRadius: 3 },
  codeBlock:         { backgroundColor: C.surface, borderRadius: 6, padding: 10, marginVertical: 4 },
  codeBlockText:     { fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace', fontSize: 12, color: C.textPrimary, lineHeight: 18 },
  cursor:            { color: C.accent, fontSize: 14, fontFamily: ARIMO },
  questionMark:      { color: C.yellow, fontWeight: '700', fontSize: 15, marginBottom: 2, fontFamily: ARIMO },
  costLabel:         { fontSize: 11, color: C.textMuted, marginTop: 4, marginLeft: 2, fontFamily: ARIMO },
  toolLine:          { fontSize: 13, color: C.textMuted, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace', marginBottom: 4, marginLeft: 2 },

  // Input bar
  completionList: { maxHeight: 180, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: C.border },
  completionItem: { paddingHorizontal: 16, paddingVertical: 10, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  completionText: { fontSize: 14, color: C.textPrimary, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace' },
  inputRow:     { flexDirection: 'row', alignItems: 'flex-end', paddingHorizontal: 12, paddingVertical: 10, paddingBottom: Platform.OS === 'android' ? 14 : 10, gap: 8, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: C.border, backgroundColor: C.surface },
  input:        { flex: 1, backgroundColor: C.bg, borderWidth: 1, borderColor: C.inputBorder, borderRadius: 12, paddingHorizontal: 14, paddingVertical: 12, color: C.textPrimary, fontSize: 15, lineHeight: 22, minHeight: 48, maxHeight: 140, fontFamily: ARIMO },
  stopBtnText:  { fontSize: 14, color: C.red, fontWeight: '600', fontFamily: ARIMO },

  // Settings header button + dropdown
  headerRight:              { flexDirection: 'row', alignItems: 'center', gap: 8 },
  settingsMenuBtn:          { paddingVertical: 4, paddingHorizontal: 6 },
  settingsMenuBtnText:      { fontSize: 18, color: C.textSecondary, letterSpacing: 1, fontFamily: ARIMO },
  containerDot:             { width: 6, height: 6, borderRadius: 3 },
  containerMenuWrap:        { position: 'absolute', top: 44, right: 0, left: 0, bottom: 0, zIndex: 100 },
  containerMenu:            { position: 'absolute', right: 12, top: 4, minWidth: 240, backgroundColor: C.bg, borderRadius: 10, borderWidth: StyleSheet.hairlineWidth, borderColor: C.border, shadowColor: '#000', shadowOpacity: 0.12, shadowRadius: 12, shadowOffset: { width: 0, height: 4 }, elevation: 8, overflow: 'hidden' },
  settingsMenuSection:      { paddingHorizontal: 14, paddingVertical: 8 },
  settingsMenuSectionTitle: { fontSize: 11, fontWeight: '700', color: C.textMuted, textTransform: 'uppercase', letterSpacing: 0.6, fontFamily: ARIMO },
  settingsMenuDivider:      { height: StyleSheet.hairlineWidth, backgroundColor: C.border },
  settingsMenuAction:       { paddingHorizontal: 14, paddingVertical: 13 },
  settingsMenuActionText:   { fontSize: 14, color: C.textSecondary, fontFamily: ARIMO },
  settingsMenuLogoutText:   { fontSize: 14, color: C.red, fontFamily: ARIMO },
  containerMenuItem:        { flexDirection: 'row', alignItems: 'center', gap: 10, paddingHorizontal: 14, paddingVertical: 12, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  containerMenuItemName:    { fontSize: 14, fontWeight: '600', color: C.textPrimary, fontFamily: ARIMO },
  containerMenuItemUrl:     { fontSize: 11, color: C.textMuted, fontFamily: ARIMO, marginTop: 1 },
  containerMenuItemStatus:  { fontSize: 11, color: C.textMuted, fontFamily: ARIMO },

  // Agent session bubble (collapsed inline)
  sessionWrap:        { paddingHorizontal: 14, marginBottom: 14 },
  sessionBox:         { borderWidth: StyleSheet.hairlineWidth, borderColor: C.border, borderRadius: 10, paddingHorizontal: 12, paddingTop: 8, paddingBottom: 10, backgroundColor: C.surface, overflow: 'hidden' },
  sessionBoxHeader:   { flexDirection: 'row', alignItems: 'center', gap: 6, marginBottom: 6 },
  sessionDot:         { width: 7, height: 7, borderRadius: 3.5 },
  sessionBoxLabel:    { flex: 1, fontSize: 11, fontWeight: '600', color: C.textSecondary, textTransform: 'uppercase', letterSpacing: 0.4, fontFamily: ARIMO },
  sessionExpandHint:  { fontSize: 13, color: C.textMuted, fontFamily: ARIMO },
  sessionScanLine:    { height: 1, backgroundColor: C.accent, opacity: 0.35, marginBottom: 6, width: 60 },
  sessionPreviewText: { fontSize: 11, color: C.textSecondary, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace', lineHeight: 16 },

  // Agent session bottom-sheet (expanded)
  sessionSheetBackdrop: { position: 'absolute', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.45)' },
  sessionSheet:         { position: 'absolute', bottom: 0, left: 0, right: 0, backgroundColor: C.bg, borderTopLeftRadius: 16, borderTopRightRadius: 16, shadowColor: '#000', shadowOpacity: 0.22, shadowRadius: 24, shadowOffset: { width: 0, height: -6 }, elevation: 20 },
  sessionSheetHandle:   { width: 36, height: 4, borderRadius: 2, backgroundColor: C.border, alignSelf: 'center', marginTop: 10, marginBottom: 2 },
  sessionModalHeader: { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: 16, paddingVertical: 12, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  sessionModalTitle:  { fontSize: 15, fontWeight: '600', color: C.textPrimary, fontFamily: ARIMO },
  sessionModalClose:  { fontSize: 16, color: C.accent, fontWeight: '500', fontFamily: ARIMO },
  sessionModalScroll: { flex: 1 },
  sessionModalText:   { fontSize: 14, color: C.textPrimary, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace', lineHeight: 22, padding: 16 },
})

