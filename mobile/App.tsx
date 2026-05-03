import AsyncStorage from '@react-native-async-storage/async-storage'
import React, { useCallback, useEffect, memo, useMemo, useRef, useState } from 'react'
import {
  ActivityIndicator,
  Animated,
  AppState,
  FlatList,
  Image,
  NativeModules,
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

interface Message {
  id:         string
  role:       'user' | 'assistant' | 'tool' | 'session' | 'interrupted'
  text:       string
  cost?:      number
  toolUseId?: string
  output?:    string
}

// ── Logging ────────────────────────────────────────────────────────────────────

const ts = () => new Date().toISOString().replace('T', ' ').slice(0, 23)
const log  = (...args: unknown[]) => console.log( `[${ts()}]`, ...args)
const logE = (...args: unknown[]) => console.error(`[${ts()}] ERROR`, ...args)

// ── Helpers ────────────────────────────────────────────────────────────────────

let _id = 0
const uid = () => `m${Date.now()}_${++_id}`

const formatCost = (usd: number) =>
  usd < 0.01 ? `$${usd.toFixed(4)}` : `$${usd.toFixed(2)}`

function parseQrData(raw: string): NoiseConnectionInfo | null {
  const parts = raw.split(':')
  log(`[qr] raw=${raw}`)
  log(`[qr] parts count=${parts.length} v=${parts[0]}`)
  if (parts[0] === '2' && parts.length === 4) {
    const [, host, portStr, pk] = parts
    const port = parseInt(portStr, 10)
    log(`[qr] parsed host=${host} port=${port} pk=${pk}`)
    if (!host || isNaN(port) || !pk) { log('[qr] parse failed: missing field'); return null }
    return { v: 2, host, port, pk }
  }
  log(`[qr] parse failed: unexpected format`)
  return null
}

// ── Dev connection ─────────────────────────────────────────────────────────────
// Fixed dev keypair baked into the server when CLAUDULHU_DEV=1.
// Public key (base32): 34577VOSZRDRTUB7XYTT6FS62Y4QYYVLQJCHP4XNDQA2763AU5YQ
//
// iOS Simulator shares the Mac's network stack — 127.0.0.1 reaches the host
// directly. Physical devices cannot reach 127.0.0.1 this way.
const isEmulator = Platform.OS === 'ios'
  ? !!((NativeModules.NoiseConnection as any)?.isSimulator)
  : Platform.OS === 'android'
    ? (() => {
        const c = NativeModules.PlatformConstants ?? {}
        const fp: string = c.Fingerprint ?? ''
        const model: string = c.Model ?? ''
        return fp.startsWith('generic') || fp.includes('emulator') ||
               model.includes('Emulator') || model.includes('Android SDK')
      })()
    : false

const DEV_HOST = '127.0.0.1'
const DEV_CONN: NoiseConnectionInfo = {
  v:     2,
  host:  DEV_HOST,
  port:  9000,
  pk:    '34577VOSZRDRTUB7XYTT6FS62Y4QYYVLQJCHP4XNDQA2763AU5YQ',
  label: 'dev (local)',
}

// ── Fonts ──────────────────────────────────────────────────────────────────────

const ARIMO  = 'Arimo'
const NUNITO = 'Nunito'

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

// Render inline markdown spans: **bold**, *italic*, _italic_, ~~strike~~, `code`
// within a single line of text (fenced code blocks already stripped before this).
function renderInlineSpans(text: string, baseStyle: object, key: React.Key): React.ReactNode {
  // Tokenise into bold / italic / strikethrough / inline-code / plain segments.
  const tokens: Array<{ kind: 'bold' | 'italic' | 'strike' | 'code' | 'plain'; value: string }> = []
  const re = /\*\*(.+?)\*\*|__(.+?)__|(?<!\*)\*(?!\*)(.+?)(?<!\*)\*(?!\*)|(?<!_)_(?!_)(.+?)(?<!_)_(?!_)|~~(.+?)~~|`([^`]+)`/gs
  let last = 0, m: RegExpExecArray | null
  while ((m = re.exec(text)) !== null) {
    if (m.index > last) tokens.push({ kind: 'plain', value: text.slice(last, m.index) })
    if      (m[1] != null) tokens.push({ kind: 'bold',   value: m[1] })
    else if (m[2] != null) tokens.push({ kind: 'bold',   value: m[2] })
    else if (m[3] != null) tokens.push({ kind: 'italic', value: m[3] })
    else if (m[4] != null) tokens.push({ kind: 'italic', value: m[4] })
    else if (m[5] != null) tokens.push({ kind: 'strike', value: m[5] })
    else if (m[6] != null) tokens.push({ kind: 'code',   value: m[6] })
    last = m.index + m[0].length
  }
  if (last < text.length) tokens.push({ kind: 'plain', value: text.slice(last) })
  if (tokens.length === 0) return null
  if (tokens.length === 1 && tokens[0].kind === 'plain') {
    return <Text key={key} style={baseStyle} selectable>{tokens[0].value}</Text>
  }
  return (
    <Text key={key} style={baseStyle} selectable>
      {tokens.map((tok, i) => {
        switch (tok.kind) {
          case 'bold':   return <Text key={i} style={{ fontWeight: '900' }} selectable>{tok.value}</Text>
          case 'italic': return <Text key={i} style={{ fontStyle: 'italic' }} selectable>{tok.value}</Text>
          case 'strike': return <Text key={i} style={{ textDecorationLine: 'line-through' }} selectable>{tok.value}</Text>
          case 'code':   return <Text key={i} style={s.inlineCode} selectable>{tok.value}</Text>
          default:       return tok.value
        }
      })}
    </Text>
  )
}

function renderText(text: string, baseStyle: object) {
  if (!text) return null

  // Split on fenced code blocks first; preserve them as opaque tokens.
  const segments = text.split(/(```[\s\S]*?```)/g)
  const elements: React.ReactNode[] = []
  let keyCounter = 0

  segments.forEach(segment => {
    // ── Fenced code block ──────────────────────────────────────────────────────
    if (segment.startsWith('```') && segment.endsWith('```')) {
      const inner = segment.slice(3, -3).replace(/^\w[^\n]*\n/, ln => {
        // strip optional language tag (e.g. ```typescript\n)
        return /^[a-zA-Z0-9_+-]+\n/.test(ln) ? '' : ln
      }).replace(/^\n/, '')
      elements.push(
        <View key={keyCounter++} style={s.codeBlock}>
          <ScrollView horizontal showsHorizontalScrollIndicator={false} keyboardShouldPersistTaps="handled">
            <Text style={s.codeBlockText} selectable>{inner}</Text>
          </ScrollView>
        </View>
      )
      return
    }

    // ── Line-by-line block-level parsing ───────────────────────────────────────
    const lines = segment.split('\n')
    let i = 0
    while (i < lines.length) {
      const line = lines[i]

      // Blank line — skip
      if (line.trim() === '') { i++; continue }

      // Heading: # / ## / ###
      const headingMatch = line.match(/^(#{1,6})\s+(.*)/)
      if (headingMatch) {
        const level = headingMatch[1].length
        const fontSize = level === 1 ? 22 : level === 2 ? 20 : 18
        const fontWeight = level <= 2 ? '700' : '600'
        const mt = level <= 2 ? 12 : 8
        elements.push(
          <Text key={keyCounter++} style={[baseStyle, { fontSize, fontWeight, marginTop: mt, marginBottom: 2 }]} selectable>
            {headingMatch[2]}
          </Text>
        )
        i++; continue
      }

      // Horizontal rule: --- / *** / ___
      if (/^(\*{3,}|-{3,}|_{3,})\s*$/.test(line)) {
        elements.push(
          <View key={keyCounter++} style={{ borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border, marginVertical: 8 }} />
        )
        i++; continue
      }

      // Blockquote: > ...
      if (line.startsWith('> ') || line === '>') {
        const quoteLines: string[] = []
        while (i < lines.length && (lines[i].startsWith('> ') || lines[i] === '>')) {
          quoteLines.push(lines[i].replace(/^>\s?/, ''))
          i++
        }
        elements.push(
          <View key={keyCounter++} style={{ borderLeftWidth: 3, borderLeftColor: C.border, paddingLeft: 10, marginVertical: 4 }}>
            {renderText(quoteLines.join('\n'), baseStyle)}
          </View>
        )
        continue
      }

      // Unordered list: lines starting with - / * / +
      if (/^[\s]*[-*+]\s/.test(line)) {
        const listItems: Array<{ indent: number; content: string }> = []
        while (i < lines.length && /^[\s]*[-*+]\s/.test(lines[i])) {
          const indentMatch = lines[i].match(/^(\s*)[-*+]\s(.*)/)
          listItems.push({ indent: Math.floor((indentMatch?.[1]?.length ?? 0) / 2), content: indentMatch?.[2] ?? '' })
          i++
        }
        elements.push(
          <View key={keyCounter++} style={{ marginVertical: 2 }}>
            {listItems.map((item, li) => (
              <View key={li} style={{ flexDirection: 'row', marginLeft: item.indent * 16, marginBottom: 2 }}>
                <Text style={[baseStyle, { marginRight: 6, lineHeight: 26 }]} selectable>•</Text>
                <View style={{ flex: 1 }}>{renderInlineSpans(item.content, baseStyle, li)}</View>
              </View>
            ))}
          </View>
        )
        continue
      }

      // Ordered list: lines starting with 1. / 2. etc.
      if (/^\s*\d+\.\s/.test(line)) {
        const listItems: Array<{ num: string; content: string }> = []
        while (i < lines.length && /^\s*\d+\.\s/.test(lines[i])) {
          const m = lines[i].match(/^\s*(\d+)\.\s(.*)/)
          listItems.push({ num: m?.[1] ?? '', content: m?.[2] ?? '' })
          i++
        }
        elements.push(
          <View key={keyCounter++} style={{ marginVertical: 2 }}>
            {listItems.map((item, li) => (
              <View key={li} style={{ flexDirection: 'row', marginBottom: 2 }}>
                <Text style={[baseStyle, { marginRight: 6, minWidth: 20, lineHeight: 26 }]} selectable>{item.num}.</Text>
                <View style={{ flex: 1 }}>{renderInlineSpans(item.content, baseStyle, li)}</View>
              </View>
            ))}
          </View>
        )
        continue
      }

      // Plain paragraph — batch consecutive non-block lines into one Text so
      // iOS selection handles can span the whole paragraph.
      const paraLines: string[] = []
      while (i < lines.length) {
        const l = lines[i]
        if (l.trim() === '') { i++; break }
        if (/^#{1,6}\s/.test(l))           break
        if (/^(\*{3,}|-{3,}|_{3,})\s*$/.test(l)) break
        if (l.startsWith('> ') || l === '>') break
        if (/^[\s]*[-*+]\s/.test(l))        break
        if (/^\s*\d+\.\s/.test(l))          break
        paraLines.push(l)
        i++
      }
      if (paraLines.length > 0) {
        const node = renderInlineSpans(paraLines.join('\n'), baseStyle, keyCounter++)
        if (node) elements.push(node)
      }
    }
  })

  return <>{elements}</>
}



// ── BlinkingCursor ────────────────────────────────────────────────────────────

function BlinkingCursor() {
  const opacity = useRef(new Animated.Value(1)).current
  useEffect(() => {
    const anim = Animated.loop(Animated.sequence([
      Animated.timing(opacity, { toValue: 0, duration: 500, useNativeDriver: true }),
      Animated.timing(opacity, { toValue: 1, duration: 500, useNativeDriver: true }),
    ]))
    anim.start()
    return () => anim.stop()
  }, [])
  return <Animated.Text style={[s.streamCursor, { opacity }]}>▍</Animated.Text>
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
      <View style={s.pendingPill}>
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

const MessageBubble = memo(function MessageBubble({
  message, prevRole, isLive,
}: {
  message:   Message
  prevRole?: Message['role']
  isLive?:   boolean
}) {
  const [toolExpanded, setToolExpanded] = useState(false)
  const fadeAnim = useRef(new Animated.Value(0)).current
  const baseTextStyle = message.role === 'user' ? s.textBlock : s.assistantTextBlock
  const renderedText = useMemo(() => renderText(message.text, baseTextStyle), [message.text, message.role])

  useEffect(() => {
    Animated.timing(fadeAnim, { toValue: 1, duration: 180, useNativeDriver: true }).start()
  }, [])

  if (message.role === 'session') return null

  // Add extra breathing room at turn boundaries (user↔assistant).
  const visiblePrev = prevRole === 'session' ? undefined : prevRole
  const turnBoundary = visiblePrev !== undefined &&
    (message.role === 'user') !== (visiblePrev === 'user')
  const extraTopMargin = turnBoundary ? 12 : 0

  if (message.role === 'interrupted') {
    return (
      <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
        <View style={[s.messageWrap, { marginBottom: 3, paddingLeft: 28 }]}>
          <Text style={s.interruptedLine} selectable>■ interrupted</Text>
        </View>
      </Animated.View>
    )
  }
  if (message.role === 'tool') {
    return (
      <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
        <TouchableOpacity
          style={[s.messageWrap, { marginBottom: 3 }]}
          onPress={() => setToolExpanded(v => !v)}
          activeOpacity={0.7}
        >
          <View style={s.toolAccent}>
            <View style={{ flexDirection: 'row', alignItems: 'center' }}>
              <Text style={[s.toolLine, { flex: 1 }]} selectable numberOfLines={toolExpanded ? undefined : 1} ellipsizeMode="tail">{message.text}</Text>
              <Text style={[s.toolChevron, { transform: [{ rotate: toolExpanded ? '90deg' : '0deg' }] }]}>›</Text>
            </View>
            {toolExpanded && message.output != null && (
              <View style={s.toolOutputBlock}>
                <ScrollView style={{ maxHeight: 180 }} nestedScrollEnabled showsVerticalScrollIndicator={false}>
                  <Text style={s.toolOutputText} selectable>{message.output}</Text>
                </ScrollView>
              </View>
            )}
          </View>
        </TouchableOpacity>
      </Animated.View>
    )
  }
  if (message.role === 'user') {
    return (
      <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
        <View style={[s.messageWrap, s.messageWrapRight]}>
          <View style={s.userBubble}>
            {renderedText}
          </View>
        </View>
      </Animated.View>
    )
  }
  return (
    <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
      <View style={s.messageWrap}>
        {renderedText}
        {isLive && <BlinkingCursor />}
        {message.cost != null && (
          <Text style={s.costLabel}>{formatCost(message.cost)}</Text>
        )}
      </View>
    </Animated.View>
  )
})

// ── AppIcon ───────────────────────────────────────────────────────────────────

function AppIcon() {
  return <Image source={require('./assets/icon.png')} style={s.creatureImg} />
}

// ── QrScanner ─────────────────────────────────────────────────────────────────

function QrScanner({ onScanned, onCancel }: { onScanned: (data: string) => void; onCancel: () => void }) {
  const { width, height } = useWindowDimensions()
  const reticleSize = Math.round(Math.min(width, height) * 0.6)
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
          <Image source={require('./assets/icon.png')} style={s.scannerIcon} />
          <Text style={s.scannerTitle}>Scan QR code</Text>
        </View>
        <View style={[s.scannerReticle, { width: reticleSize, height: reticleSize }]}>
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

  const [messages,       setMessages]       = useState<Message[]>([])
  const [status,         setStatus]         = useState<ConnStatus>('connecting')
  const [input,          setInput]          = useState(initialDraft ?? '')
  const draftKey = `draft:${baseUrl}`
  const [completions,    setCompletions]    = useState<string[]>([])
  const [showScrollBtn,  setShowScrollBtn]  = useState(false)
  const [inputAreaH,     setInputAreaH]     = useState(0)
  const [streamingMsgId, setStreamingMsgId] = useState<string | null>(null)
  const [stopSent,       setStopSent]       = useState(false)

  const sendMessageRef    = useRef<() => void>(() => {})
  const wsRef             = useRef<WebSocket | null>(null)
  const listRef           = useRef<FlatList<Message>>(null)
  const isAtBottomRef     = useRef(true)
  const contentHeightRef  = useRef(0)
  const listHeightRef     = useRef(0)
  const lastToolIdRef     = useRef<string | null>(null)
  const historyAbortRef   = useRef<AbortController | null>(null)
  const messagesRef       = useRef<Message[]>([])

  useEffect(() => { messagesRef.current = messages }, [messages])

  const updateStatus = useCallback((s: ConnStatus) => {
    setStatus(s)
    onStatusChange(s)
  }, [onStatusChange])

  // Shared event handler used by both sendMessage and reattachStream.
  const handleStreamEvent = useCallback((
    raw: string,
    opts: {
      streamingIdRef:    { current: string }
      hasAssistantMsgRef: { current: boolean }
      onDone:            () => void
      onFirstChunk?:     (id: string) => void
      onStreamEnd?:      () => void
    },
  ) => {
    let event: { type: string; text?: string; tool?: string; input?: unknown; cost_usd?: number; message?: string; line?: string; tool_use_id?: string; output?: string }
    try { event = JSON.parse(raw) } catch { return }

    if (event.type === 'text' && event.text) {
      const chunk = event.text
      if (!opts.hasAssistantMsgRef.current) {
        opts.hasAssistantMsgRef.current = true
        opts.onFirstChunk?.(opts.streamingIdRef.current)
        setMessages(prev => [...prev, { id: opts.streamingIdRef.current, role: 'assistant' as const, text: chunk }])
      } else {
        setMessages(prev => prev.map(m => m.id === opts.streamingIdRef.current ? { ...m, text: m.text + chunk } : m))
      }
    } else if (event.type === 'tool_use') {
      opts.hasAssistantMsgRef.current = false
      opts.streamingIdRef.current = uid()
      const firstVal = event.input && typeof event.input === 'object'
        ? String(Object.values(event.input as Record<string, unknown>)[0] ?? '').trim()
        : ''
      const toolText = firstVal ? `${event.tool}(${firstVal})` : (event.tool ?? '')
      log(`[chat] tool_use tool=${event.tool}`)
      const toolId = uid()
      lastToolIdRef.current = toolId
      setMessages(prev => [...prev, { id: toolId, role: 'tool' as const, text: toolText, toolUseId: event.tool_use_id }])
    } else if (event.type === 'tool_output' && event.line != null) {
      const toolId = lastToolIdRef.current
      if (toolId) {
        setMessages(prev => prev.map(m =>
          m.id === toolId ? { ...m, output: (m.output ?? '') + event.line + '\n' } : m
        ))
      }
    } else if (event.type === 'tool_result' && event.output != null) {
      const toolId = lastToolIdRef.current
      if (toolId) {
        setMessages(prev => prev.map(m =>
          m.id === toolId ? { ...m, output: typeof event.output === 'string' ? event.output : JSON.stringify(event.output) } : m
        ))
      }
    } else if (event.type === 'done') {
      log(`[chat] stream done cost_usd=${event.cost_usd}`)
      lastToolIdRef.current = null
      wsRef.current = null
      opts.onStreamEnd?.()
      opts.onDone()
    } else if (event.type === 'interrupted') {
      log(`[chat] stream interrupted cost_usd=${event.cost_usd}`)
      lastToolIdRef.current = null
      wsRef.current = null
      opts.onStreamEnd?.()
      opts.onDone()
    } else if (event.type === 'error') {
      logE(`[chat] stream error: ${event.message}`)
      lastToolIdRef.current = null
      wsRef.current = null
      opts.onStreamEnd?.()
      setMessages(prev => [...prev, { id: uid(), role: 'assistant' as const, text: `\u2717 ${event.message ?? 'error'}` }])
      updateStatus('ready')
    }
  }, [updateStatus])

  // Open a watch-only WebSocket to tail an already-running server loop.
  const reattachStream = useCallback(() => {
    if (wsRef.current) {
      log('[chat] reattachStream: already have ws, skipping')
      return
    }
    const streamingIdRef      = { current: uid() }
    const hasAssistantMsgRef  = { current: false }
    const wsUrl = baseUrl.replace(/^http/, 'ws') + '/stream'
    log(`[chat] reattachStream opening ${wsUrl}`)
    const wsStart = Date.now()
    const ws = new WebSocket(wsUrl)
    wsRef.current = ws
    updateStatus('streaming')
    ws.onopen = () => {
      log(`[chat] reattachStream ws open after ${Date.now() - wsStart}ms, sending watch`)
      ws.send(JSON.stringify({ type: 'watch' }))
    }
    ws.onmessage = (e) => {
      handleStreamEvent(e.data, {
        streamingIdRef,
        hasAssistantMsgRef,
        onDone:       () => loadHistoryRef.current(),
        onFirstChunk: (id) => setStreamingMsgId(id),
        onStreamEnd:  () => setStreamingMsgId(null),
      })
    }
    ws.onerror = (e) => {
      logE(`[chat] reattachStream ws error after ${Date.now() - wsStart}ms: ${JSON.stringify(e)}`)
      wsRef.current = null
      setStreamingMsgId(null)
      updateStatus('error')
    }
    ws.onclose = (e) => {
      log(`[chat] reattachStream ws closed after ${Date.now() - wsStart}ms code=${e.code} reason=${e.reason}`)
    }
  }, [baseUrl, handleStreamEvent, updateStatus])

  // Keep a stable ref to loadHistory so reattachStream can call it without
  // being listed as a dependency (avoids circular dep: loadHistory → reattachStream → loadHistory).
  const loadHistoryRef = useRef<() => void>(() => {})

  const loadHistory = useCallback(() => {
    historyAbortRef.current?.abort()
    const controller = new AbortController()
    historyAbortRef.current = controller
    log(`[chat] loadHistory GET ${baseUrl}/history`)
    fetch(`${baseUrl}/history`, { signal: controller.signal })
      .then(r => { log(`[chat] loadHistory HTTP ${r.status}`); return r.json() })
      .then((data: { messages: Array<{ role: string; text: string; cost_usd?: number; output?: string }>; is_streaming?: boolean }) => {
        log(`[chat] history loaded ${data.messages.length} messages is_streaming=${data.is_streaming}`)
        const msgs: Message[] = data.messages.map((m, i) => ({
          id:   `h${i}`,
          role: m.role as Message['role'],
          text: m.text,
          ...(m.cost_usd != null ? { cost: m.cost_usd } : {}),
          ...(m.output    != null ? { output: m.output } : {}),
        }))
        const prev = messagesRef.current
        const unchanged = prev.length === msgs.length && msgs.every((m, i) =>
          m.role === prev[i].role && m.text === prev[i].text &&
          m.cost === prev[i].cost && m.output === prev[i].output
        )
        if (!unchanged) setMessages(msgs)
        if (data.is_streaming) {
          reattachStream()
        } else {
          updateStatus('ready')
        }
        setTimeout(() => {
          const offset = Math.max(0, contentHeightRef.current - listHeightRef.current)
          listRef.current?.scrollToOffset({ offset, animated: false })
        }, 50)
      })
      .catch(e => {
        if ((e as Error).name === 'AbortError') return
        logE(`[chat] loadHistory failed: ${String(e)}`)
        updateStatus('error')
      })
  }, [baseUrl, reattachStream, updateStatus])

  useEffect(() => { loadHistoryRef.current = loadHistory }, [loadHistory])

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
    return () => { historyAbortRef.current?.abort() }
  }, [baseUrl])

  // Re-fetch history when app foregrounds (tunnel may have reconnected).
  useEffect(() => {
    const sub = AppState.addEventListener('change', nextState => {
      if (nextState === 'active') {
        // Close any stale WS before reloading history so reattachStream always
        // opens a fresh connection (the old one may still appear open on iOS
        // after backgrounding, causing reattachStream to bail out early and
        // leaving the screen blank after setMessages replaces the message list).
        if (wsRef.current) {
          wsRef.current.close()
          wsRef.current = null
        }
        loadHistory()
      }
    })
    return () => sub.remove()
  }, [loadHistory])

  // @ completions
  useEffect(() => {
    const parsed = parseAtQuery(input)
    if (!parsed) { setCompletions([]); return }
    let cancelled = false
    const timer = setTimeout(() => {
      fetch(`${baseUrl}/completions?dir_part=${encodeURIComponent(parsed.dirPart)}&file_part=${encodeURIComponent(parsed.filePart)}`)
        .then(r => r.json())
        .then((data: string[]) => { if (!cancelled) setCompletions(data) })
        .catch(() => { if (!cancelled) setCompletions([]) })
    }, 200)
    return () => { cancelled = true; clearTimeout(timer) }
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

    log(`[chat] sendMessage (${text.length} chars): ${text.slice(0, 80)}`)
    setMessages(prev => [...prev, { id: uid(), role: 'user' as const, text }])
    isAtBottomRef.current = true
    setInput('')
    AsyncStorage.removeItem(draftKey).catch(() => {})
    updateStatus('streaming')

    const streamingIdRef     = { current: uid() }
    const hasAssistantMsgRef = { current: false }

    const wsUrl = baseUrl.replace(/^http/, 'ws') + '/stream'
    log(`[chat] opening ws ${wsUrl}`)
    const wsSendStart = Date.now()
    const ws = new WebSocket(wsUrl)
    wsRef.current = ws
    ws.onopen = () => {
      log(`[chat] ws open after ${Date.now() - wsSendStart}ms, sending message`)
      ws.send(JSON.stringify({ text }))
    }
    ws.onmessage = (e) => {
      handleStreamEvent(e.data, {
        streamingIdRef,
        hasAssistantMsgRef,
        onDone:       () => loadHistoryRef.current(),
        onFirstChunk: (id) => setStreamingMsgId(id),
        onStreamEnd:  () => setStreamingMsgId(null),
      })
    }
    ws.onerror = (e) => {
      logE(`[chat] ws error after ${Date.now() - wsSendStart}ms: ${JSON.stringify(e)}`)
      wsRef.current = null
      setStreamingMsgId(null)
      setMessages(prev => [...prev, { id: uid(), role: 'assistant' as const, text: '\u2717 network error' }])
      updateStatus('error')
    }
    ws.onclose = (e) => {
      log(`[chat] ws closed after ${Date.now() - wsSendStart}ms code=${e.code} reason=${e.reason}`)
    }
  }, [input, status, baseUrl, handleStreamEvent, updateStatus])

  sendMessageRef.current = sendMessage

  const clearConversation = useCallback(() => {
    fetch(`${baseUrl}/clear`, { method: 'POST' })
      .then(() => { setMessages([]); updateStatus('ready') })
      .catch(() => loadHistoryRef.current())
  }, [baseUrl])
  clearRef.current = clearConversation

  const isPending = status === 'streaming'
  useEffect(() => { if (!isPending) setStopSent(false) }, [isPending])

  const renderMessageItem = useCallback(({ item, index }: { item: Message; index: number }) => (
    <MessageBubble
      message={item}
      prevRole={index > 0 ? messages[index - 1].role : undefined}
      isLive={item.id === streamingMsgId}
    />
  ), [messages, streamingMsgId])

  return (
    <View style={s.pane}>
      <View style={{ flex: 1 }}>
        <FlatList
          ref={listRef}
          data={messages}
          keyExtractor={m => m.id}
          renderItem={renderMessageItem}
          contentContainerStyle={[
            s.messageListContent,
            { paddingBottom: inputAreaH + 8 },
            (status === 'connecting' || status === 'error') && { paddingTop: 34 },
          ]}
          style={s.messageList}
          ListEmptyComponent={
            <View style={s.emptyStateWrap}>
              <Text style={s.emptyStateName}>claudulhu</Text>
              <Text style={s.emptyState}>you can just do things anon</Text>
            </View>
          }
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


        {completions.length > 0 && (
          <ScrollView
            style={[s.completionList, { bottom: inputAreaH }]}
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
        <View style={s.inputFloat} onLayout={e => setInputAreaH(e.nativeEvent.layout.height)}>
          {isPending ? (
            <TouchableOpacity
              style={[s.inputStopBtn, stopSent && { opacity: 0.4 }]}
              disabled={stopSent}
              onPress={() => {
                setStopSent(true)
                const ws = wsRef.current
                if (ws) {
                  ws.send(JSON.stringify({ type: 'interrupt' }))
                  const toolId = lastToolIdRef.current
                  if (toolId) {
                    setMessages(prev => prev.map(m =>
                      m.id === toolId ? { ...m, role: 'interrupted' as const } : m
                    ))
                    lastToolIdRef.current = null
                  }
                }
              }}
              activeOpacity={0.7}
            >
              <Text style={s.stopBtnText}>■  stop</Text>
            </TouchableOpacity>
          ) : (
            <View style={s.inputRow}>
              <TextInput
                style={s.input}
                value={input}
                onChangeText={setInput}
                placeholder="message…"
                placeholderTextColor={C.textMuted}
                multiline
                blurOnSubmit={false}
              />
              <TouchableOpacity
                style={[s.sendBtn, !input.trim() && s.sendBtnDisabled]}
                onPress={() => sendMessageRef.current()}
                disabled={!input.trim()}
                activeOpacity={0.75}
              >
                <Text style={s.sendBtnIcon}>↑</Text>
              </TouchableOpacity>
            </View>
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

        {(status === 'connecting' || status === 'error') && (
          <View style={[s.reconnectBanner, { backgroundColor: status === 'error' ? '#fee2e2' : '#fffbeb', borderBottomColor: status === 'error' ? '#fecaca' : '#fef3c7' }]} pointerEvents="none">
            {status === 'connecting' && <ActivityIndicator color={C.yellow} size="small" style={{ marginRight: 6 }} />}
            <Text style={[s.reconnectText, { color: status === 'error' ? C.red : C.yellow }]}>
              {status === 'error' ? 'Connection error' : 'Connecting…'}
            </Text>
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
      if (json) { try { saved = JSON.parse(json) } catch (e) { logE(`[app] failed to parse saved connection: ${e}`) } }
      if (!saved && __DEV__ && isEmulator) { log('[app] no saved connection, using DEV_CONN'); saved = DEV_CONN }
      if (saved) log(`[app] loaded connection host=${saved.host} port=${saved.port} pk=${saved.pk.slice(0, 8)}…`)
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

    if (!target) { log('[noise] no target, skipping connect'); return }
    if (!NoiseConnection) {
      logE('[noise] native module unavailable')
      setTunnelError('Native Noise module unavailable')
      return
    }

    if (activeChild) {
      log(`[noise] connecting to child "${activeChild.name}" host=${target.host} port=${target.port} pk=${target.pk.slice(0, 8)}… status=${activeChild.status}`)
    } else {
      log(`[noise] connecting to master host=${target.host} port=${target.port} pk=${target.pk.slice(0, 8)}…`)
    }

    let live = true
    log(`[noise] full pk=${target.pk}`)
    log(`[noise] calling disconnect() before connect`)
    NoiseConnection.disconnect()
    log(`[noise] calling connect() host=${target.host} port=${target.port}`)
    const connectStart = Date.now()
    NoiseConnection.connect(target.host, target.port, target.pk)
      .then(port => {
        log(`[noise] connect() resolved in ${Date.now() - connectStart}ms → local port ${port}`)
        if (!live) { log('[noise] connect resolved but effect already cleaned up — discarding'); return }
        setTunnelPort(port)
      })
      .catch(e => {
        logE(`[noise] connect() rejected in ${Date.now() - connectStart}ms: ${e?.message ?? String(e)}`)
        if (!live) return
        if (activeChild) {
          setActiveChild(null)
        } else {
          setTunnelError(e?.message ?? String(e))
        }
      })

    return () => {
      live = false
      log('[noise] effect cleanup: calling disconnect()')
      NoiseConnection?.disconnect()
    }
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
      .then(r => { log(`[app] fetchContainers HTTP ${r.status}`); return r.json() })
      .then((data: { containers: ContainerInfo[] }) => {
        log(`[app] fetchContainers: ${data.containers.length} container(s)`)
        data.containers.forEach(c => {
          log(`[app]   container id=${c.id} name=${c.name} status=${c.status} host=${c.host} port=${c.port} pubkey=${c.pubkey ? c.pubkey.slice(0, 8) + '…' : '(none)'}`)
        })
        setContainers(data.containers)
        const waitingId = startingContainerIdRef.current
        if (waitingId) {
          const started = data.containers.find(c => c.id === waitingId && c.status === 'running' && c.pubkey)
          if (started) {
            log(`[app] container ${started.name} is now running, connecting`)
            startingContainerIdRef.current = null
            setStartingContainerId(null)
            setStartingError(null)
            setTunnelPort(null)
            setActiveChild(started)
          }
        }
      })
      .catch(e => logE(`[app] fetchContainers failed: ${String(e)}`))
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
    log(`[qr] scanned raw=${raw}`)
    const parsed = parseQrData(raw)
    if (!parsed) {
      logE(`[qr] parse failed for: ${raw}`)
      setTunnelError('Invalid QR code')
      return
    }
    log(`[qr] parsed host=${parsed.host} port=${parsed.port} pk=${parsed.pk.slice(0, 8)}…`)
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
    log(`[app] startContainer id=${id}`)
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
          logE(`[app] startContainer error: ${data.error}`)
          startingContainerIdRef.current = null
          setStartingContainerId(null)
          setStartingError(data.error)
        } else {
          log(`[app] startContainer request accepted, polling for running state`)
        }
      })
      .catch(e => {
        logE(`[app] startContainer request failed: ${String(e)}`)
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
          <AppIcon />
          <Text style={s.setupTitle}>claudulhu</Text>
          {tunnelError
            ? <Text style={s.setupError}>{tunnelError}</Text>
            : <ActivityIndicator color={C.accent} size="small" style={{ marginTop: 8 }} />
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
          <AppIcon />
          <Text style={s.setupTitle}>claudulhu</Text>
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
                        setTunnelPort(null)
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
  setupSafe:    { flex: 1, backgroundColor: '#F7F7F8' },
  setupCenter:  { flex: 1, alignItems: 'center', justifyContent: 'center', paddingHorizontal: 40, gap: 16 },
  setupTitle:   { fontSize: 26, fontWeight: '700', color: C.textPrimary, letterSpacing: 2, fontFamily: NUNITO },
  setupDesc:    { fontSize: 15, color: C.textSecondary, textAlign: 'center', lineHeight: 22, fontFamily: ARIMO },
  setupStatus:  { fontSize: 15, color: C.textSecondary, textAlign: 'center', fontFamily: ARIMO },
  setupError:   { fontSize: 14, color: C.red, textAlign: 'center', lineHeight: 20, fontFamily: ARIMO },
  setupBtn:     { backgroundColor: '#D16E50', borderRadius: 12, paddingVertical: 14, paddingHorizontal: 32, alignItems: 'center', marginTop: 8 },
  setupBtnText: { color: '#F7F7F8', fontWeight: '700', fontSize: 16, fontFamily: NUNITO },

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
  scannerIcon:       { width: 64, height: 64, borderRadius: 14, marginBottom: 8 },
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
  emptyStateWrap:    { alignItems: 'center', marginTop: 80, gap: 6 },
  emptyStateName:    { fontSize: 22, fontWeight: '700', color: C.textMuted, letterSpacing: 2, fontFamily: ARIMO },
  emptyState:        { textAlign: 'center', color: C.textMuted, fontSize: 14, fontFamily: ARIMO },
  reconnectBanner:   { position: 'absolute', top: 0, left: 0, right: 0, flexDirection: 'row', alignItems: 'center', justifyContent: 'center', paddingVertical: 6, borderBottomWidth: StyleSheet.hairlineWidth, zIndex: 10 },
  reconnectText:     { fontSize: 12, fontWeight: '600', fontFamily: ARIMO },

  // Scroll-to-bottom button
  scrollBtnWrap:     { position: 'absolute', left: 0, right: 0, alignItems: 'center', pointerEvents: 'box-none' },
  scrollBtn:         { backgroundColor: C.bg, borderRadius: 20, width: 36, height: 36, alignItems: 'center', justifyContent: 'center', shadowColor: '#000', shadowOpacity: 0.15, shadowRadius: 6, shadowOffset: { width: 0, height: 2 }, elevation: 4, borderWidth: StyleSheet.hairlineWidth, borderColor: C.border, marginBottom: 8 },
  scrollBtnIcon:     { fontSize: 18, color: C.textSecondary, lineHeight: 22, fontFamily: ARIMO },

  // Messages
  messageWrap:      { paddingHorizontal: 14, marginBottom: 14 },
  messageWrapRight: { alignItems: 'flex-end' },
  userBubble:          { backgroundColor: C.surface, borderRadius: 18, borderBottomRightRadius: 4, paddingHorizontal: 14, paddingVertical: 10, maxWidth: '80%' },
  textBlock:           { color: C.textPrimary, fontSize: 18, lineHeight: 26, fontWeight: '400', fontFamily: ARIMO },
  assistantTextBlock:  { color: C.textPrimary, fontSize: 16, lineHeight: 24, fontWeight: '400', fontFamily: ARIMO },
  inlineCode:        { fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace', fontSize: 13, color: C.textPrimary, backgroundColor: C.surface, paddingHorizontal: 3, paddingVertical: 1, borderRadius: 3 },
  codeBlock:         { backgroundColor: C.surface, borderRadius: 6, padding: 10, marginVertical: 4 },
  codeBlockText:     { fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace', fontSize: 12, color: C.textPrimary, lineHeight: 18 },
  cursor:            { color: C.textMuted, fontSize: 14, fontFamily: ARIMO },
  streamCursor:      { color: C.accent, fontSize: 16, marginTop: 2 },
  pendingPill:       { flexDirection: 'row', alignItems: 'center', gap: 4, backgroundColor: C.surface, borderRadius: 12, paddingHorizontal: 12, paddingVertical: 8, alignSelf: 'flex-start' },
  questionMark:      { color: C.yellow, fontWeight: '700', fontSize: 15, marginBottom: 2, fontFamily: ARIMO },
  costLabel:         { fontSize: 11, color: C.textMuted, marginTop: 4, marginLeft: 2, fontFamily: ARIMO },
  toolAccent:        { borderLeftWidth: 2, borderLeftColor: C.border, paddingLeft: 8 },
  toolLine:          { fontSize: 14, color: C.textMuted, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace' },
  toolChevron:       { fontSize: 16, color: C.textMuted, marginLeft: 6, fontWeight: '300' },
  toolOutputBlock:   { marginTop: 6, borderLeftWidth: 2, borderLeftColor: C.border, paddingLeft: 10 },
  toolOutputText:    { fontSize: 12, color: C.textSecondary, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace', lineHeight: 18 },
  interruptedLine:   { fontSize: 16, lineHeight: 24, color: C.textMuted, fontFamily: ARIMO, fontStyle: 'italic' },

  // Input bar
  completionList: { position: 'absolute', left: 0, right: 0, maxHeight: 180, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: C.border, backgroundColor: C.bg, zIndex: 10, elevation: 10 },
  completionItem: { paddingHorizontal: 16, paddingVertical: 10, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  completionText: { fontSize: 14, color: C.textPrimary, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace' },
  inputFloat:   { position: 'absolute', bottom: 0, left: 0, right: 0, paddingHorizontal: 12, paddingBottom: 12 },
  inputRow:     { flexDirection: 'row', alignItems: 'flex-end', gap: 8 },
  input:        { flex: 1, backgroundColor: C.bg, borderWidth: 1, borderColor: C.inputBorder, borderRadius: 24, paddingHorizontal: 20, paddingVertical: 16, color: C.textPrimary, fontSize: 18, lineHeight: 26, minHeight: 56, maxHeight: 140, fontFamily: ARIMO, shadowColor: '#000', shadowOpacity: 0.08, shadowRadius: 12, shadowOffset: { width: 0, height: 2 }, elevation: 4 },
  sendBtn:      { width: 48, height: 48, borderRadius: 24, backgroundColor: C.accent, alignItems: 'center', justifyContent: 'center', marginBottom: 4, shadowColor: '#000', shadowOpacity: 0.15, shadowRadius: 8, shadowOffset: { width: 0, height: 2 }, elevation: 4 },
  sendBtnDisabled: { backgroundColor: C.inputBorder },
  sendBtnIcon:  { fontSize: 22, color: '#fff', fontWeight: '700', lineHeight: 26 },
  inputStopBtn: { backgroundColor: C.bg, borderWidth: 1, borderColor: C.inputBorder, borderRadius: 24, paddingHorizontal: 20, paddingVertical: 16, minHeight: 56, alignItems: 'center', justifyContent: 'center', shadowColor: '#000', shadowOpacity: 0.08, shadowRadius: 12, shadowOffset: { width: 0, height: 2 }, elevation: 4 },
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
  settingsMenuLogoutText:   { fontSize: 15, color: C.red, fontFamily: ARIMO },
  containerMenuItem:        { flexDirection: 'row', alignItems: 'center', gap: 10, paddingHorizontal: 14, paddingVertical: 12, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  containerMenuItemName:    { fontSize: 17, fontWeight: '600', color: C.textPrimary, fontFamily: ARIMO },
  containerMenuItemUrl:     { fontSize: 13, color: C.textMuted, fontFamily: ARIMO, marginTop: 1 },
  containerMenuItemStatus:  { fontSize: 13, color: C.textMuted, fontFamily: ARIMO },

})
