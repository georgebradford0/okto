import AsyncStorage from '@react-native-async-storage/async-storage'
import React, { useCallback, useEffect, memo, useMemo, useRef, useState } from 'react'
import {
  ActivityIndicator,
  Alert,
  Animated,
  AppState,
  Easing,
  FlatList,
  Image,
  Keyboard,
  NativeModules,
  PermissionsAndroid,
  Platform,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  TouchableOpacity,
  useWindowDimensions,
  Vibration,
  View,
} from 'react-native'
import { KeyboardProvider, useReanimatedKeyboardAnimation } from 'react-native-keyboard-controller'
import Reanimated, { useAnimatedStyle } from 'react-native-reanimated'
import { SafeAreaProvider, SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context'
import { Camera, useCameraDevice, useCodeScanner } from 'react-native-vision-camera'
import NoiseConnection from './src/NativeNoiseConnection'
import { registerWithRelay } from './src/registerWithRelay'
import {
  type ClientFrame,
  type AgentInfo as WireAgentInfo,
  type ServerEvent,
  type TaskRecord,
  encodeClientFrame,
  parseServerEvent,
} from './src/wire'

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
  status:  string
  kind:    string // 'local' | 'remote'
}

interface Message {
  id:         string
  role:       'user' | 'assistant' | 'tool' | 'session' | 'interrupted' | 'error' | 'bg_complete'
  text:       string
  cost?:      number
  toolUseId?: string
  output?:    string
  prevRole?:  Message['role']
}

// ── Logging ────────────────────────────────────────────────────────────────────

const ts = () => new Date().toISOString().replace('T', ' ').slice(0, 23)
const log  = (...args: unknown[]) => console.log( `[${ts()}]`, ...args)
const logE = (...args: unknown[]) => console.error(`[${ts()}] ERROR`, ...args)

// ── Helpers ────────────────────────────────────────────────────────────────────

let _id = 0
const uid = () => `m${Date.now()}_${++_id}`

/** Stamp prevRole on every message so renderItem never needs to close over the full array. */
const withPrevRoles = (msgs: Message[]): Message[] =>
  msgs.map((m, i) => ({ ...m, prevRole: i > 0 ? msgs[i - 1].role : undefined }))

/** Append one message to an existing array and re-stamp only the new entry's prevRole. */
const appendMsg = (prev: Message[], msg: Message): Message[] => {
  const stamped = { ...msg, prevRole: prev.length > 0 ? prev[prev.length - 1].role : undefined }
  return [...prev, stamped]
}

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
// Fixed dev keypair baked into the server when OCTO_DEV=1.
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
const MONO   = Platform.OS === 'ios' ? 'Menlo' : 'monospace'

// ── Colours ────────────────────────────────────────────────────────────────────
// "Editorial Terminal" — warm cream paper, deep ink-navy, oceanic teal accent.
// The palette references both marine field-research (octo / octopus) and the
// printed page: ink on paper instead of pixels on glass.

const C = {
  bg:            '#F4EFE3',  // warm cream paper
  surface:       '#EBE4D2',  // deeper cream surface (code blocks, raised tints)
  border:        '#D6CDB6',  // hairline divider
  accent:        '#0B6E73',  // deep oceanic teal — "live" / tools / accents
  accentLight:   '#DEE9E5',  // teal-tinted cream
  green:         '#0B6E73',  // alias of accent: "live" status
  yellow:        '#8E6B14',  // dark goldenrod: warnings / connecting
  red:           '#7E2926',  // oxblood: errors / destructive
  textPrimary:   '#0E1A24',  // deep ink-navy with the faintest cool undertone
  textSecondary: '#4F5763',  // body grey
  textMuted:     '#8E8775',  // muted warm grey: placeholders / status meta
  inputBorder:   '#D6CDB6',
}

const statusColor = (st: ConnStatus): string => {
  if (st === 'ready')     return C.green
  if (st === 'streaming') return C.accent
  if (st === 'error')     return C.red
  return C.yellow
}

// ── Text rendering ─────────────────────────────────────────────────────────────

// Render inline markdown spans: **bold**, *italic*, ~~strike~~, `code`
// within a single line of text (fenced code blocks already stripped before this).
// Note: _italic_ is intentionally not supported to avoid false matches on snake_case identifiers.
function renderInlineSpans(text: string, baseStyle: object, key: React.Key): React.ReactNode {
  // Tokenise into bold / italic / strikethrough / inline-code / plain segments.
  const tokens: Array<{ kind: 'bold' | 'italic' | 'strike' | 'code' | 'plain'; value: string }> = []
  const re = /\*\*(.+?)\*\*|__(.+?)__|(?<!\*)\*(?!\*)(.+?)(?<!\*)\*(?!\*)|~~(.+?)~~|`([^`]+)`/gs
  let last = 0, m: RegExpExecArray | null
  while ((m = re.exec(text)) !== null) {
    if (m.index > last) tokens.push({ kind: 'plain', value: text.slice(last, m.index) })
    if      (m[1] != null) tokens.push({ kind: 'bold',   value: m[1] })
    else if (m[2] != null) tokens.push({ kind: 'bold',   value: m[2] })
    else if (m[3] != null) tokens.push({ kind: 'italic', value: m[3] })
    else if (m[4] != null) tokens.push({ kind: 'strike', value: m[4] })
    else if (m[5] != null) tokens.push({ kind: 'code',   value: m[5] })
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
      let lang = ''
      const inner = segment.slice(3, -3).replace(/^\w[^\n]*\n/, ln => {
        // strip optional language tag (e.g. ```typescript\n)
        if (/^[a-zA-Z0-9_+-]+\n/.test(ln)) { lang = ln.trim(); return '' }
        return ln
      }).replace(/^\n/, '')
      elements.push(
        <View key={keyCounter++} style={s.codeBlock}>
          {lang ? <Text style={s.codeBlockLang}>{lang}</Text> : null}
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





// ── OrbitingArc ───────────────────────────────────────────────────────────────
// A quarter-circle arc that revolves continuously. Implemented as the border
// of an empty rounded-square View: only the top edge is colored, the others
// are transparent, so the visible stroke is the top quadrant of the ring.
// Rotating the whole View carries the arc around the perimeter — a classic
// native-only spinner shape, visually heavier than a single dot.

function OrbitingArc({ size = 48, thickness = 3, durationMs = 1100 }: {
  size?:        number
  thickness?:   number
  durationMs?:  number
}) {
  const rotate = useRef(new Animated.Value(0)).current
  useEffect(() => {
    const loop = Animated.loop(
      Animated.timing(rotate, {
        toValue: 1,
        duration: durationMs,
        easing: Easing.linear,
        useNativeDriver: true,
      }),
    )
    loop.start()
    return () => loop.stop()
  }, [durationMs])
  const spin = rotate.interpolate({ inputRange: [0, 1], outputRange: ['0deg', '360deg'] })
  return (
    <Animated.View
      pointerEvents="none"
      style={[
        s.orbitArc,
        {
          width: size,
          height: size,
          borderRadius: size / 2,
          borderWidth: thickness,
          transform: [{ rotate: spin }],
        },
      ]}
    />
  )
}

// ── PaperPlane ────────────────────────────────────────────────────────────────
// The canonical "send" mark, rendered with two stacked View triangles:
//   1. A large right-pointing triangle in white — the wing silhouette.
//   2. A smaller triangle in the button's background colour, sitting on the
//      bottom edge — this "notch" creates the V-cut that reads as the
//      paper-plane fold.
// The whole assembly is rotated -22° so it reads as a plane in flight, not a
// play-button. When disabled the wing dims and the notch tracks the disabled
// surface colour so the cutout still works.

function PaperPlane({ disabled = false }: { disabled?: boolean }) {
  const wingColor  = disabled ? C.textMuted : '#FFFFFF'
  const notchColor = disabled ? C.surface  : '#4A90E2'
  return (
    <View style={s.paperPlaneWrap}>
      <View style={s.paperPlaneTilt}>
        <View style={[s.paperPlaneWing,  { borderLeftColor: wingColor }]} />
        <View style={[s.paperPlaneNotch, { borderLeftColor: notchColor }]} />
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
  return name.replace(/^lair-/, '')
}

// ── MessageBubble ─────────────────────────────────────────────────────────────

const MessageBubble = memo(function MessageBubble({
  message, prevRole,
}: {
  message:   Message
  prevRole?: Message['role']
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
  // Tighter bottom margin for consecutive same-role messages (tool runs, etc.)
  const sameRole = visiblePrev !== undefined && visiblePrev === message.role
  const bubbleBottomMargin = sameRole ? 4 : 14

  if (message.role === 'error') {
    return (
      <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
        <View style={[s.messageWrap, { marginBottom: bubbleBottomMargin, paddingLeft: 28 }]}>
          <Text style={s.errorLine} selectable>⚠ {message.text}</Text>
        </View>
      </Animated.View>
    )
  }
  if (message.role === 'interrupted') {
    return (
      <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
        <View style={[s.messageWrap, { marginBottom: bubbleBottomMargin, paddingLeft: 28 }]}>
          <Text style={s.interruptedLine} selectable>■ interrupted</Text>
        </View>
      </Animated.View>
    )
  }
  if (message.role === 'bg_complete') {
    // Take just the first line of the injected text — it's prefixed with
    // "Background task <id> completed (status=…)" which is enough context;
    // the long body would crowd the chip.
    const firstLine = message.text.split('\n', 1)[0] || message.text
    return (
      <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
        <View style={[s.messageWrap, { marginBottom: bubbleBottomMargin, paddingLeft: 28 }]}>
          <Text style={s.bgCompleteLine} selectable>◇ {firstLine}</Text>
        </View>
      </Animated.View>
    )
  }
  if (message.role === 'tool') {
    return (
      <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
        <TouchableOpacity
          style={[s.messageWrap, { marginBottom: 4 }]}
          onPress={() => setToolExpanded(v => !v)}
          activeOpacity={0.7}
        >
          <View style={s.toolChip}>
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
        <View style={[s.messageWrap, s.messageWrapRight, { marginBottom: bubbleBottomMargin }]}>
          <View style={s.userBubble}>
            {renderedText}
          </View>
        </View>
      </Animated.View>
    )
  }
  return (
    <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
      <View style={[s.messageWrap, { marginBottom: bubbleBottomMargin }]}>
        {renderedText}
        {message.cost != null && (
          <Text style={s.costLabel}>{formatCost(message.cost)}</Text>
        )}
      </View>
    </Animated.View>
  )
})

// ── Tasks UI helpers ──────────────────────────────────────────────────────────

function relativeTime(epochSecs: number): string {
  const delta = Math.max(0, Math.floor(Date.now() / 1000) - epochSecs)
  if (delta < 60)        return `${delta}s ago`
  if (delta < 3600)      return `${Math.floor(delta / 60)}m ago`
  if (delta < 86400)     return `${Math.floor(delta / 3600)}h ago`
  return `${Math.floor(delta / 86400)}d ago`
}

function taskStatusColor(status: TaskRecord['status']): string {
  if (status === 'running')   return C.accent
  if (status === 'done')      return C.green
  if (status === 'cancelled') return C.textMuted
  return C.red
}

function TasksHeaderButton({ tasks, onPress }: { tasks: TaskRecord[]; onPress: () => void }) {
  const runningCount = tasks.filter(t => t.status === 'running').length
  return (
    <TouchableOpacity
      style={s.tasksBtn}
      onPress={onPress}
      hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
    >
      <View style={[s.tasksBtnDot, runningCount > 0 ? { backgroundColor: C.accent } : { backgroundColor: C.textMuted }]} />
      <Text style={s.tasksBtnText}>
        {runningCount > 0 ? `TASKS · ${runningCount}` : 'TASKS'}
      </Text>
    </TouchableOpacity>
  )
}

const TaskRow = memo(function TaskRow({ task, onCancel }: { task: TaskRecord; onCancel: () => void }) {
  const [expanded, setExpanded] = useState(false)
  const isRunning = task.status === 'running'
  const ts = task.completed_at != null ? relativeTime(task.completed_at) : relativeTime(task.started_at)
  return (
    <View style={s.taskRow}>
      <View style={s.taskRowHeader}>
        <View style={[s.taskStatusTag, { borderColor: taskStatusColor(task.status) }]}>
          <View style={[s.taskStatusDot, { backgroundColor: taskStatusColor(task.status) }]} />
          <Text style={[s.taskStatusLabel, { color: taskStatusColor(task.status) }]}>
            {task.status.toUpperCase()}
          </Text>
        </View>
        <Text style={s.taskTimestamp}>{ts}</Text>
        {isRunning && (
          <TouchableOpacity style={s.taskStopBtn} onPress={onCancel} hitSlop={{ top: 6, bottom: 6, left: 6, right: 6 }}>
            <Text style={s.taskStopText}>STOP</Text>
          </TouchableOpacity>
        )}
      </View>
      <TouchableOpacity activeOpacity={0.7} onPress={() => setExpanded(v => !v)}>
        <Text style={s.taskDescription} numberOfLines={expanded ? undefined : 2} selectable>
          {task.command}
        </Text>
        {task.summary != null && task.summary.length > 0 && (
          <Text style={s.taskSummary} numberOfLines={expanded ? undefined : 2} selectable>
            {task.summary}
          </Text>
        )}
        {task.cost_usd != null && task.cost_usd > 0 && (
          <Text style={s.taskCost}>{formatCost(task.cost_usd)}</Text>
        )}
      </TouchableOpacity>
    </View>
  )
})

function TasksModal({ visible, tasks, onClose, onCancel }: {
  visible:  boolean
  tasks:    TaskRecord[]
  onClose:  () => void
  onCancel: (taskId: string) => void
}) {
  const insets = useSafeAreaInsets()
  const slide  = useRef(new Animated.Value(0)).current
  const [mounted, setMounted] = useState(false)

  useEffect(() => {
    if (visible) setMounted(true)
    Animated.timing(slide, {
      toValue:        visible ? 1 : 0,
      duration:       240,
      useNativeDriver: true,
    }).start(({ finished }) => {
      if (finished && !visible) setMounted(false)
    })
  }, [visible])

  if (!mounted) return null

  // Sort: running first, then most-recently started
  const sorted = tasks.slice().sort((a, b) => {
    if (a.status === 'running' && b.status !== 'running') return -1
    if (b.status === 'running' && a.status !== 'running') return 1
    return b.started_at - a.started_at
  })

  return (
    <View style={StyleSheet.absoluteFill} pointerEvents="box-none">
      <Animated.View
        style={[s.tasksBackdrop, { opacity: slide }]}
        pointerEvents={visible ? 'auto' : 'none'}
      >
        <TouchableOpacity style={StyleSheet.absoluteFill} activeOpacity={1} onPress={onClose} />
      </Animated.View>
      <Animated.View
        style={[
          s.tasksSheet,
          { paddingBottom: insets.bottom + 16,
            transform: [{ translateY: slide.interpolate({ inputRange: [0, 1], outputRange: [600, 0] }) }] },
        ]}
      >
        <View style={s.tasksHandle} />
        <View style={s.tasksHeader}>
          <Text style={s.tasksHeaderTitle}>Background Tasks</Text>
          <TouchableOpacity onPress={onClose} hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}>
            <Text style={s.sidebarCloseIcon}>✕</Text>
          </TouchableOpacity>
        </View>
        <ScrollView
          style={{ flex: 1 }}
          contentContainerStyle={{ paddingBottom: 20 }}
          showsVerticalScrollIndicator={false}
        >
          {sorted.length === 0 ? (
            <View style={s.tasksEmptyWrap}>
              <Text style={s.tasksEmptyText}>No background tasks</Text>
            </View>
          ) : sorted.map(t => (
            <TaskRow key={t.task_id} task={t} onCancel={() => onCancel(t.task_id)} />
          ))}
        </ScrollView>
      </Animated.View>
    </View>
  )
}

// ── AppIcon ───────────────────────────────────────────────────────────────────

function AppIcon({ pulse = false }: { pulse?: boolean }) {
  const scale = useRef(new Animated.Value(1)).current
  useEffect(() => {
    if (!pulse) return
    const anim = Animated.loop(
      Animated.sequence([
        Animated.timing(scale, { toValue: 1.06, duration: 900, useNativeDriver: true }),
        Animated.timing(scale, { toValue: 1,    duration: 900, useNativeDriver: true }),
      ])
    )
    anim.start()
    return () => anim.stop()
  }, [pulse, scale])
  return (
    <Animated.Image
      source={require('./assets/icon.png')}
      style={[s.creatureImg, { transform: [{ scale }] }]}
    />
  )
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
          <Text style={s.scannerTitle}>Scan Session QR</Text>
          <Text style={s.scannerSubtitle}>Point at the code shown by your octo server</Text>
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
  baseUrl, onStatusChange, clearRef, initialDraft, onDraftChange, reconnectingRef, reloadRef, closeWsRef,
  sendFrameRef, onContainersUpdate, onTasksUpdate,
}: {
  baseUrl:             string
  onStatusChange:      (s: ConnStatus) => void
  clearRef:            React.MutableRefObject<() => void>
  initialDraft?:       string
  onDraftChange?:      (draft: string) => void
  reconnectingRef?:    React.MutableRefObject<boolean>
  reloadRef?:          React.MutableRefObject<() => void>
  closeWsRef?:         React.MutableRefObject<() => void>
  /// Imperative handle: call to send a typed client frame on the persistent
  /// /stream WS. Returns false if the WS isn't currently open. Master ChatPane
  /// receives this so AppInner can issue start_container frames.
  sendFrameRef?:       React.MutableRefObject<(frame: ClientFrame) => boolean>
  /// Push hook for `agents` events. Lair sends one immediately after Ready
  /// and again on every poller state-change. Children never send agents.
  onContainersUpdate?: (agents: WireAgentInfo[]) => void
  /// Push hook for `tasks` events. Both lair and agent send one on /stream
  /// open and after every spawn / completion / cancellation. The list is the
  /// per-chat background-task registry — see core::TaskRecord.
  onTasksUpdate?:      (tasks: TaskRecord[]) => void
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
  const [stopSent,       setStopSent]       = useState(false)
  // Holds the baseUrl that /history has successfully loaded for. Used by the
  // persistent /stream effect to gate WS open until history is in place — if
  // the WS opens first and the server replays buffered events for an in-flight
  // turn, the subsequent history reconcile can clobber the streaming bubble.
  const [historyReadyFor, setHistoryReadyFor] = useState<string | null>(null)
  const stopAckTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  const sendMessageRef    = useRef<() => void>(() => {})
  const wsRef             = useRef<WebSocket | null>(null)
  const closingRef        = useRef(false)
  const streamingIdRef    = useRef<string>(uid())
  const hasAssistantMsgRef = useRef<boolean>(false)
  // True for the duration of an agentic turn after the user message has been
  // scrolled into view: suppresses content-grew auto-scroll so streaming text
  // doesn't drag the viewport down while the user is reading. Released only
  // when the turn ends (done / interrupted / error). User scroll never
  // releases — they can read or peek freely without forfeiting the lock.
  const streamingLockRef    = useRef<boolean>(false)
  // One-shot flag set in sendMessage and consumed by onContentSizeChange the
  // very next time it auto-scrolls — engages streamingLockRef immediately
  // after the user message is on screen.
  const scrollOnceAndLockRef = useRef<boolean>(false)
  // Per-WS counters for the mobile→server keepalive. The WS effect emits
  // `ping { id: ++clientPingNextRef }` every KEEPALIVE_INTERVAL_MS and the
  // server replies with `pong { id }` (handled in handleStreamEvent above,
  // bumps clientPongAckedRef). If clientPingNextRef - clientPongAckedRef
  // reaches KEEPALIVE_MAX_MISSED, we force-close the WS so the existing
  // backoff reconnect kicks in. Symmetrical to the server-side check.
  const clientPingNextRef    = useRef<number>(0)
  const clientPongAckedRef   = useRef<number>(0)
  const listRef           = useRef<FlatList<Message>>(null)
  const isAtBottomRef     = useRef(true)
  const contentHeightRef  = useRef(0)
  const listHeightRef     = useRef(0)
  const lastToolIdRef     = useRef<string | null>(null)
  const historyAbortRef   = useRef<AbortController | null>(null)
  const messagesRef       = useRef<Message[]>([])

  useEffect(() => { messagesRef.current = messages }, [messages])

  // Expose imperative handles to the parent.
  useEffect(() => {
    if (closeWsRef) closeWsRef.current = () => {
      if (wsRef.current) {
        closingRef.current = true
        wsRef.current.close()
        wsRef.current = null
      }
    }
  }, [closeWsRef])

  const updateStatus = useCallback((s: ConnStatus) => {
    setStatus(s)
    onStatusChange(s)
  }, [onStatusChange])

  // Send a typed client frame on the persistent WS. Returns false if the WS
  // isn't currently open (caller decides how to surface that).
  const sendFrame = useCallback((frame: ClientFrame): boolean => {
    const ws = wsRef.current
    if (!ws || ws.readyState !== WebSocket.OPEN) return false
    ws.send(encodeClientFrame(frame))
    return true
  }, [])

  // Expose sendFrame so the parent (AppInner) can issue frames like start_container
  // on the master ChatPane's WS without owning the WS itself.
  useEffect(() => {
    if (sendFrameRef) sendFrameRef.current = sendFrame
  }, [sendFrame, sendFrameRef])

  const handleStreamEvent = useCallback((raw: string) => {
    const event = parseServerEvent(raw)
    if (!event) return

    switch (event.type) {
      case 'ready': {
        // Server greets us; status becomes 'streaming' if we joined an in-flight
        // turn (events for it will arrive next), else 'ready' for input.
        updateStatus(event.resumed ? 'streaming' : 'ready')
        if (event.resumed) {
          // The server's buffer holds the *entire* in-flight turn from its
          // first event and is about to be replayed. If we've already rendered
          // any of it (mid-turn WS reconnect), naively processing the replay
          // would duplicate every text chunk and tool row. Truncate back to
          // the last turn anchor — the user/bg_complete row that triggered
          // this turn — so the replay rebuilds the in-flight portion cleanly.
          setMessages(prev => {
            for (let i = prev.length - 1; i >= 0; i--) {
              const role = prev[i].role
              if (role === 'user' || role === 'bg_complete') {
                return prev.slice(0, i + 1)
              }
            }
            return prev
          })
          lastToolIdRef.current = null
        }
        // Reset per-turn streaming refs unconditionally: for resumed=false this
        // is the first turn after connect; for resumed=true the replay restarts
        // the turn from its first event.
        streamingIdRef.current = uid()
        hasAssistantMsgRef.current = false
        break
      }
      case 'text': {
        const chunk = event.text
        if (!hasAssistantMsgRef.current) {
          hasAssistantMsgRef.current = true
          const id = streamingIdRef.current
          setMessages(prev => appendMsg(prev, { id, role: 'assistant' as const, text: chunk }))
        } else {
          const id = streamingIdRef.current
          setMessages(prev => prev.map(m => m.id === id ? { ...m, text: m.text + chunk } : m))
        }
        break
      }
      case 'tool_use': {
        // Bump streaming id so the *next* text block becomes a fresh message
        // after the tool, not appended to pre-tool text.
        hasAssistantMsgRef.current = false
        streamingIdRef.current = uid()
        const firstVal = event.input && typeof event.input === 'object'
          ? String(Object.values(event.input as Record<string, unknown>)[0] ?? '').trim()
          : ''
        const label = event.display ?? event.tool
        const toolText = firstVal ? `${label} (${firstVal})` : label
        log(`[chat] tool_use tool=${event.tool}`)
        const toolId = uid()
        lastToolIdRef.current = toolId
        setMessages(prev => appendMsg(prev, { id: toolId, role: 'tool' as const, text: toolText }))
        break
      }
      case 'tool_output': {
        const toolId = lastToolIdRef.current
        if (toolId) {
          setMessages(prev => prev.map(m =>
            m.id === toolId ? { ...m, output: (m.output ?? '') + event.line + '\n' } : m
          ))
        }
        break
      }
      case 'tool_result': {
        const toolId = lastToolIdRef.current
        if (toolId) {
          const out = typeof event.output === 'string' ? event.output : JSON.stringify(event.output)
          setMessages(prev => prev.map(m => m.id === toolId ? { ...m, output: out } : m))
        }
        break
      }
      case 'done': {
        log(`[chat] stream done cost_usd=${event.cost_usd}`)
        lastToolIdRef.current = null
        streamingLockRef.current = false
        updateStatus('ready')
        const cost = event.cost_usd
        setMessages(prev => {
          for (let i = prev.length - 1; i >= 0; i--) {
            if (prev[i].role === 'assistant') {
              const next = prev.slice()
              next[i] = { ...next[i], cost }
              return next
            }
          }
          return prev
        })
        // Turn boundary — anything streamed after this (auto-turn from a
        // bg_complete, or the next user turn) must start a fresh assistant
        // bubble, not append to the one we just sealed.
        hasAssistantMsgRef.current = false
        streamingIdRef.current = uid()
        break
      }
      case 'interrupt_ack':
        log('[chat] interrupt acknowledged by server')
        if (stopAckTimerRef.current) {
          clearTimeout(stopAckTimerRef.current)
          stopAckTimerRef.current = null
        }
        break
      case 'interrupted': {
        log(`[chat] stream interrupted cost_usd=${event.cost_usd}`)
        lastToolIdRef.current = null
        streamingLockRef.current = false
        updateStatus('ready')
        const cost = event.cost_usd
        setMessages(prev => {
          let stamped = prev
          for (let i = prev.length - 1; i >= 0; i--) {
            if (prev[i].role === 'assistant') {
              stamped = prev.slice()
              stamped[i] = { ...stamped[i], cost }
              break
            }
          }
          return appendMsg(stamped, { id: uid(), role: 'interrupted' as const, text: 'interrupted' })
        })
        hasAssistantMsgRef.current = false
        streamingIdRef.current = uid()
        break
      }
      case 'error':
        logE(`[chat] stream error: ${event.message}`)
        lastToolIdRef.current = null
        streamingLockRef.current = false
        setMessages(prev => appendMsg(prev, { id: uid(), role: 'error' as const, text: event.message }))
        updateStatus('ready')
        hasAssistantMsgRef.current = false
        streamingIdRef.current = uid()
        break
      case 'system':
        log(`[chat] system: ${event.text}`)
        break
      case 'agents':
        if (onContainersUpdate) onContainersUpdate(event.agents)
        break
      case 'tasks':
        if (onTasksUpdate) onTasksUpdate(event.tasks)
        break
      case 'bg_complete': {
        // Live insertion of the bg_complete chip between assistant turns. The
        // id is stable per task so a subsequent /history reload (which also
        // contains this row) is a no-op rather than a duplicate.
        const id = `bg_${event.task_id}`
        setMessages(prev => prev.some(m => m.id === id)
          ? prev
          : appendMsg(prev, { id, role: 'bg_complete' as const, text: event.text }))
        break
      }
      case 'ping':
        sendFrame({ type: 'pong', id: event.id })
        break
      case 'pong':
        // Reply to one of our pings — bumps the per-WS ack tracker so the
        // mobile-side liveness checker stops counting it as outstanding.
        if (event.id > clientPongAckedRef.current) {
          clientPongAckedRef.current = event.id
        }
        break
    }
  }, [updateStatus, sendFrame, onContainersUpdate, onTasksUpdate])

  // Keep a stable ref to loadHistory so reattachStream can call it without
  // being listed as a dependency (avoids circular dep: loadHistory → reattachStream → loadHistory).
  const loadHistoryRef = useRef<() => void>(() => {})

  const loadHistory = useCallback((attempt = 0) => {
    historyAbortRef.current?.abort()
    const controller = new AbortController()
    historyAbortRef.current = controller
    log(`[chat] loadHistory GET ${baseUrl}/history${attempt > 0 ? ` (attempt ${attempt + 1})` : ''}`)
    fetch(`${baseUrl}/history`, { signal: controller.signal })
      .then(r => { log(`[chat] loadHistory HTTP ${r.status}`); return r.json() })
      .then((data: { messages: Array<{ role: string; text: string; cost_usd?: number; output?: string }> }) => {
        log(`[chat] history loaded ${data.messages.length} messages`)
        // Mark history loaded for this baseUrl so the gated WS effect can run.
        setHistoryReadyFor(baseUrl)
        // First successful tunnel round-trip — register for push notifications
        // (idempotent, swallows all errors). Fires on the first chat mount per
        // baseUrl; subsequent calls are short-circuited by an in-module Set.
        registerWithRelay(baseUrl, log).catch(() => {})
        const msgs: Message[] = withPrevRoles(data.messages.map((m, i) => ({
          id:   `h${i}`,
          role: m.role as Message['role'],
          text: m.text,
          ...(m.cost_usd != null ? { cost: m.cost_usd } : {}),
          ...(m.output    != null ? { output: m.output } : {}),
        })))
        const prev = messagesRef.current
        const eq = (a: Message, b: Message) =>
          a.role === b.role && a.text === b.text && a.cost === b.cost && a.output === b.output
        const prefixMatch = prev.length <= msgs.length && prev.every((m, i) => eq(m, msgs[i]))
        if (!prefixMatch) {
          // History diverged (clear/edit) — full replace
          setMessages(msgs)
        } else if (msgs.length > prev.length) {
          // Server added messages — append only, preserving existing ids
          const tail = msgs.slice(prev.length)
          setMessages(cur => [...cur, ...tail.map((m, j) => ({ ...m, prevRole: j === 0 ? (cur.length > 0 ? cur[cur.length - 1].role : undefined) : tail[j - 1].role }))])
        }
        // else identical — no update.
        // Status is driven entirely by /stream events now (`ready` on connect,
        // `done`/`interrupted`/`error` at turn end), so loadHistory no longer
        // needs to drive it from `is_streaming`.
        setTimeout(() => {
          // Only re-pin to the bottom if the user was already there. Otherwise
          // the user is reading earlier content and an autoscroll on every
          // history reconcile (e.g. end-of-turn) would yank them away.
          if (!isAtBottomRef.current) return
          const offset = Math.max(0, contentHeightRef.current - listHeightRef.current)
          listRef.current?.scrollToOffset({ offset, animated: false })
        }, 50)
      })
      .catch(e => {
        if ((e as Error).name === 'AbortError') return
        logE(`[chat] loadHistory failed: ${String(e)}`)
        // Retry once after a short delay — the native Noise proxy may not be
        // ready to accept connections immediately after the tunnel reconnects
        // (e.g. on foreground return), which would cause a spurious error flash.
        if (attempt === 0) {
          setTimeout(() => {
            if (historyAbortRef.current === controller) loadHistory(1)
          }, 600)
        } else {
          updateStatus('error')
        }
      })
  }, [baseUrl, updateStatus])

  useEffect(() => { loadHistoryRef.current = loadHistory }, [loadHistory])
  useEffect(() => { if (reloadRef) reloadRef.current = loadHistory }, [loadHistory, reloadRef])

  // Persistent /stream WebSocket with exponential-backoff reconnect.
  //
  // Opens once per baseUrl and stays open across turns. On unintentional close
  // (network drop, server eviction, NAT timeout) it auto-reconnects with
  // exponential backoff capped at 30s; the counter resets the moment the
  // server's first `ready` event arrives. Intentional closes (effect cleanup
  // on baseUrl change / unmount, parent-driven closeWsRef teardown) flag
  // closingRef to suppress the retry loop.
  //
  // Gated on `historyReadyFor === baseUrl`: if the WS were to open while
  // /history is still in flight and the server replays buffered events for an
  // in-flight turn (child watch path), the subsequent history reconcile can
  // clobber the streaming bubble. Loading history first puts the canonical
  // conversation in place before any deltas land.
  useEffect(() => {
    if (historyReadyFor !== baseUrl) return
    const wsUrl = baseUrl.replace(/^http/, 'ws') + '/stream'
    let cancelled = false
    let currentWs: WebSocket | null = null
    let retryTimer: ReturnType<typeof setTimeout> | null = null
    let pingTimer: ReturnType<typeof setInterval> | null = null

    const BASE_BACKOFF_MS = 1000
    const MAX_BACKOFF_MS  = 30_000
    // Mirror of core::KEEPALIVE_INTERVAL / KEEPALIVE_MAX_MISSED. Don't tighten
    // these without bumping the server-side defense; the server tolerates 30s
    // (2 × 15s) of silence before evicting and we want our own check to fire
    // strictly before that so we drop into reconnect first.
    const KEEPALIVE_INTERVAL_MS = 15_000
    const KEEPALIVE_MAX_MISSED  = 2
    let attempt = 0

    const connect = () => {
      if (cancelled) return
      log(`[chat] connecting ws ${wsUrl} (attempt ${attempt + 1})`)
      const wsStart = Date.now()
      const ws = new WebSocket(wsUrl)
      currentWs = ws
      wsRef.current = ws

      ws.onopen = () => {
        log(`[chat] ws open after ${Date.now() - wsStart}ms`)
        // We don't reset `attempt` here — wait for the server's `ready` frame
        // to confirm the channel is actually usable, not just that TCP/WS opened.
        // Reset per-connection ping counters and start firing pings.
        clientPingNextRef.current  = 0
        clientPongAckedRef.current = 0
        if (pingTimer) clearInterval(pingTimer)
        pingTimer = setInterval(() => {
          if (ws.readyState !== WebSocket.OPEN) return
          // Outstanding = pings sent but not yet acked by server.
          const outstanding = clientPingNextRef.current - clientPongAckedRef.current
          if (outstanding >= KEEPALIVE_MAX_MISSED) {
            logE(`[chat] keepalive: ${outstanding} unacked ping(s) — closing WS`)
            // Close intentionally; the existing onclose path will reconnect
            // with backoff (and we mustn't suppress that, so leave closingRef
            // alone — this is a connection problem, not a teardown).
            ws.close()
            return
          }
          const id = ++clientPingNextRef.current
          ws.send(encodeClientFrame({ type: 'ping', id }))
        }, KEEPALIVE_INTERVAL_MS)
      }
      ws.onmessage = (e) => {
        const raw = typeof e.data === 'string' ? e.data : ''
        if (raw) {
          // Reset backoff on first sign of a real conversation: the server's
          // `ready` greeting. Anything earlier (e.g. pre-Ready noise) doesn't
          // count as a successful session.
          if (attempt > 0 && raw.includes('"ready"')) {
            attempt = 0
          }
          handleStreamEvent(raw)
        }
      }
      ws.onerror = (e) => {
        // Don't surface error UI on transient drops — onclose will fire and
        // schedule a retry. Only the foreground-return path ever sets status='error'.
        logE(`[chat] ws error after ${Date.now() - wsStart}ms: ${JSON.stringify(e)}`)
      }
      ws.onclose = (e) => {
        log(`[chat] ws closed after ${Date.now() - wsStart}ms code=${e.code} reason=${e.reason}`)
        if (wsRef.current === ws) wsRef.current = null
        currentWs = null
        if (pingTimer) { clearInterval(pingTimer); pingTimer = null }
        const intentional = closingRef.current
        closingRef.current = false
        if (cancelled || intentional) return
        const delay = Math.min(BASE_BACKOFF_MS * Math.pow(2, attempt), MAX_BACKOFF_MS)
        attempt += 1
        log(`[chat] reconnect scheduled in ${delay}ms (next attempt #${attempt + 1})`)
        retryTimer = setTimeout(() => {
          retryTimer = null
          connect()
        }, delay)
      }
    }

    connect()

    return () => {
      cancelled = true
      if (retryTimer) {
        clearTimeout(retryTimer)
        retryTimer = null
      }
      if (pingTimer) {
        clearInterval(pingTimer)
        pingTimer = null
      }
      if (currentWs) {
        log('[chat] tearing down ws (effect cleanup)')
        closingRef.current = true
        currentWs.close()
        wsRef.current = null
      }
    }
  }, [baseUrl, historyReadyFor, handleStreamEvent, reconnectingRef, updateStatus])

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
  // Skip the 'connecting' flash when a silent reconnect is in progress.
  useEffect(() => {
    const silent = reconnectingRef?.current ?? false
    if (!silent) updateStatus('connecting')
    // Re-gate the WS effect for the new baseUrl until /history finishes
    // loading for it.
    setHistoryReadyFor(null)
    loadHistory()
    return () => { historyAbortRef.current?.abort() }
  }, [baseUrl])

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
    setMessages(prev => appendMsg(prev, { id: uid(), role: 'user' as const, text }))
    isAtBottomRef.current = true
    setInput('')
    Keyboard.dismiss()
    AsyncStorage.removeItem(draftKey).catch(() => {})
    updateStatus('streaming')

    // Reset per-turn refs so the upcoming text/tool events anchor onto a fresh
    // streaming message id rather than appending onto the previous turn's tail.
    streamingIdRef.current = uid()
    hasAssistantMsgRef.current = false
    // Release any lingering lock from a previous turn, then arm the one-shot
    // so the user msg auto-scrolls into view and the lock re-engages right
    // after — see onContentSizeChange.
    streamingLockRef.current = false
    scrollOnceAndLockRef.current = true

    if (!sendFrame({ type: 'user_message', text })) {
      logE('[chat] sendMessage: WS not open, surfacing error')
      setMessages(prev => appendMsg(prev, { id: uid(), role: 'error' as const, text: 'network error' }))
      updateStatus('error')
    }
  }, [input, status, sendFrame, updateStatus, draftKey])

  sendMessageRef.current = sendMessage

  const clearConversation = useCallback(() => {
    fetch(`${baseUrl}/clear`, { method: 'POST' })
      .then(() => { setMessages([]); updateStatus('ready') })
      .catch(() => loadHistoryRef.current())
  }, [baseUrl])
  clearRef.current = clearConversation

  const isPending = status === 'streaming'
  useEffect(() => {
    if (!isPending) {
      setStopSent(false)
      if (stopAckTimerRef.current) {
        clearTimeout(stopAckTimerRef.current)
        stopAckTimerRef.current = null
      }
    }
  }, [isPending])

  const renderMessageItem = useCallback(({ item }: { item: Message }) => (
    <MessageBubble
      message={item}
      prevRole={item.prevRole}
    />
  ), [])

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
            status === 'error' && { paddingTop: 34 },
          ]}
          style={s.messageList}
          ListEmptyComponent={
            <View style={s.emptyStateWrap}>
              <AppIcon />
              <View style={s.emptyStateRule} />
              <Text style={s.emptyStateTagline}>Awaiting Instructions</Text>
            </View>
          }
          onContentSizeChange={(_, h) => {
            contentHeightRef.current = h
            if (streamingLockRef.current) {
              if (isAtBottomRef.current && h > listHeightRef.current) {
                isAtBottomRef.current = false
                setShowScrollBtn(true)
              }
              return
            }
            if (isAtBottomRef.current) {
              const offset = Math.max(0, h - listHeightRef.current)
              listRef.current?.scrollToOffset({ offset, animated: false })
              if (scrollOnceAndLockRef.current) {
                scrollOnceAndLockRef.current = false
                streamingLockRef.current = true
              }
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
          <View style={s.inputRow}>
            <TextInput
              // Lock height to one line when empty so a `setInput('')` on send
              // collapses the box immediately — without this, iOS keeps the
              // previous multiline intrinsic size for ~a second. With a value
              // present we omit `height` so RN's intrinsic sizing handles
              // multiline auto-grow (clamped by minHeight/maxHeight in s.input).
              style={[s.input, !input && { height: 56 }]}
              value={input}
              onChangeText={setInput}
              placeholder="message…"
              placeholderTextColor={C.textMuted}
              multiline
              blurOnSubmit={false}
            />
            {isPending ? (
              // Streaming: send → stop button at the center of a moving-circle
              // (single dot orbiting). Tapping issues an interrupt and locks
              // the button at reduced opacity until the server's
              // interrupt_ack (or the 3 s timeout fallback in stopAckTimerRef).
              <View style={s.inputBtnSlot}>
                <OrbitingArc size={56} thickness={3} />
                <TouchableOpacity
                  style={[s.stopBtnInline, stopSent && { opacity: 0.4 }]}
                  disabled={stopSent}
                  onPress={() => {
                    if (!sendFrame({ type: 'interrupt' })) return
                    setStopSent(true)
                    if (stopAckTimerRef.current) clearTimeout(stopAckTimerRef.current)
                    stopAckTimerRef.current = setTimeout(() => {
                      stopAckTimerRef.current = null
                      setStopSent(false)
                    }, 3000)
                  }}
                  activeOpacity={0.7}
                >
                  <Text style={s.stopBtnInlineIcon}>■</Text>
                </TouchableOpacity>
              </View>
            ) : (
              <TouchableOpacity
                style={[s.sendBtn, !input.trim() && s.sendBtnDisabled]}
                onPress={() => sendMessageRef.current()}
                disabled={!input.trim()}
                activeOpacity={0.75}
              >
                <PaperPlane disabled={!input.trim()} />
              </TouchableOpacity>
            )}
          </View>
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

        {status === 'error' && (
          <View style={[s.reconnectBanner, { backgroundColor: C.surface, borderBottomColor: C.red }]} pointerEvents="none">
            <Text style={[s.reconnectText, { color: C.red }]}>Connection error</Text>
          </View>
        )}
      </View>
      <Reanimated.View style={[{ backgroundColor: C.surface }, spacerStyle]} />
    </View>
  )
})


// ── ChildChatScreen ───────────────────────────────────────────────────────────

function ChildChatScreen({ child, tunnelPort, tunnelError, onClose, initialDraft, onDraftChange, reconnectingRef, reloadRef, closeWsRef }: {
  child:             ContainerInfo
  tunnelPort:        number | null
  tunnelError:       string | null
  onClose:           () => void
  initialDraft?:     string
  onDraftChange?:    (draft: string) => void
  reconnectingRef?:  React.MutableRefObject<boolean>
  reloadRef?:        React.MutableRefObject<() => void>
  closeWsRef?:       React.MutableRefObject<() => void>
}) {
  const [chatStatus, setChatStatus] = useState<ConnStatus>('connecting')
  const [tasks,      setTasks]      = useState<TaskRecord[]>([])
  const [showTasksModal, setShowTasksModal] = useState(false)
  const clearRef     = useRef<() => void>(() => {})
  const sendFrameRef = useRef<(frame: ClientFrame) => boolean>(() => false)

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
            <View style={[s.connStatusPill, { backgroundColor: statusColor(chatStatus) + '22' }]}>
              <View style={[s.connDot, { backgroundColor: statusColor(chatStatus) }]} />
              <Text style={[s.connPillLabel, { color: statusColor(chatStatus) }]}>{chatStatus === 'ready' || chatStatus === 'streaming' ? 'live' : chatStatus}</Text>
            </View>
            <Text style={s.headerTitle}>{containerDisplayName(child.name)}</Text>
          </View>
          <View style={s.headerRight}>
            <TasksHeaderButton tasks={tasks} onPress={() => setShowTasksModal(true)} />
            <TouchableOpacity
              style={s.clearBtn}
              onPress={() => clearRef.current()}
              hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
              disabled={chatStatus !== 'ready'}
            >
              <Text style={[s.clearBtnText, chatStatus !== 'ready' && { opacity: 0.3 }]}>clear</Text>
            </TouchableOpacity>
          </View>
        </View>

        {tunnelPort ? (
          <ChatPane
            baseUrl={`http://127.0.0.1:${tunnelPort}/agents/${child.name}`}
            onStatusChange={setChatStatus}
            clearRef={clearRef}
            initialDraft={initialDraft}
            onDraftChange={onDraftChange}
            reconnectingRef={reconnectingRef}
            reloadRef={reloadRef}
            closeWsRef={closeWsRef}
            sendFrameRef={sendFrameRef}
            onTasksUpdate={setTasks}
          />
        ) : tunnelError ? (
          <View style={s.setupCenter}>
            <Text style={[s.setupError, { color: C.red }]}>{tunnelError}</Text>
          </View>
        ) : null}

        <TasksModal
          visible={showTasksModal}
          tasks={tasks}
          onClose={() => setShowTasksModal(false)}
          onCancel={(id) => {
            log(`[child] cancel_task id=${id}`)
            sendFrameRef.current({ type: 'cancel_task', id })
          }}
        />
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
  // Per-chat background-task registry for the master chat. Pushed by lair on
  // /stream open and after every spawn / completion / cancellation.
  const [masterTasks,         setMasterTasks]         = useState<TaskRecord[]>([])
  const [showTasksModal,      setShowTasksModal]      = useState(false)
  const [showSidebar,    setShowSidebar]    = useState(false)
  const sidebarAnim = useRef(new Animated.Value(0)).current
  const [startingContainerId, setStartingContainerId] = useState<string | null>(null)
  const [startingError,       setStartingError]       = useState<string | null>(null)
  const [reconnecting,        setReconnecting]        = useState(false)
  const startingContainerIdRef = useRef<string | null>(null)
  const clearChatRef       = useRef<() => void>(() => {})
  const reloadRef          = useRef<() => void>(() => {})
  const closeWsRef         = useRef<() => void>(() => {})
  // Bound to the master ChatPane's persistent /stream WS once it's open.
  // Returns false if no WS is connected (caller should surface or retry).
  const masterSendFrameRef = useRef<(frame: ClientFrame) => boolean>(() => false)
  // In-memory draft cache: survives ChatPane unmount/remount without async latency.
  const draftsRef          = useRef<Record<string, string>>({})
  // Held true for the full duration of a foreground-return reconnect so that
  // WS error/close callbacks know not to surface a connection-error to the user.
  const reconnectingRef    = useRef<boolean>(false)
  // Ref mirrors of conn/activeChild so the imperative reconnect() can read
  // current values without being a useCallback dependency.
  const connRef            = useRef<NoiseConnectionInfo | null>(null)
  const activeChildRef     = useRef<ContainerInfo | null>(null)
  useEffect(() => { connRef.current = conn },         [conn])
  useEffect(() => { activeChildRef.current = activeChild }, [activeChild])

  // The single Noise tunnel always points at lair. Mobile reaches a child
  // agent's chat by opening WS to `/agents/<name>/stream` over the same
  // tunnel — lair proxies the traffic.
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

  // Connection effect — owns the single Noise tunnel to lair. The tunnel is
  // *not* re-established when switching between the master chat and a child
  // chat: lair proxies child traffic over the same tunnel via per-agent
  // URLs. Foreground-return reconnects are handled imperatively by reconnect().
  useEffect(() => {
    setTunnelError(null)

    const target = conn ? { host: conn.host, port: conn.port, pk: conn.pk } : null

    if (!target) {
      log('[noise] no target, skipping connect')
      setTunnelPort(null)
      return
    }

    if (!NoiseConnection) {
      logE('[noise] native module unavailable')
      setTunnelError('Native Noise module unavailable')
      return
    }

    setTunnelPort(null)
    log(`[noise] connect host=${target.host} port=${target.port} pk=${target.pk.slice(0, 8)}…`)

    let live = true
    NoiseConnection.disconnect()
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
        setTunnelError(e?.message ?? String(e))
      })

    return () => {
      live = false
      log('[noise] effect cleanup: calling disconnect()')
      NoiseConnection?.disconnect()
    }
  }, [conn])

  // Imperative reconnect — called on foreground return. Reconnects the single
  // Noise tunnel to lair; whichever chat is open (master or child) just
  // re-attaches its WS to the new tunnel port.
  const reconnectRef = useRef<() => Promise<void>>(async () => {})
  reconnectRef.current = async () => {
    const target = connRef.current
      ? { host: connRef.current.host, port: connRef.current.port, pk: connRef.current.pk }
      : null

    if (!target || !NoiseConnection) return

    log(`[noise] foreground reconnect host=${target.host} port=${target.port} pk=${target.pk.slice(0, 8)}…`)
    reconnectingRef.current = true
    setReconnecting(true)

    closeWsRef.current()

    try {
      NoiseConnection.disconnect()
      const connectStart = Date.now()
      const port = await NoiseConnection.connect(target.host, target.port, target.pk)
      log(`[noise] foreground reconnect resolved in ${Date.now() - connectStart}ms → local port ${port}`)
      setTunnelPort(port)
      reloadRef.current()
    } catch (e: unknown) {
      logE(`[noise] foreground reconnect failed: ${(e as Error)?.message ?? String(e)}`)
      setTunnelPort(null)
      setTunnelError((e as Error)?.message ?? String(e))
    } finally {
      reconnectingRef.current = false
      setReconnecting(false)
    }
  }

  // Single AppState listener — calls the imperative reconnect on foreground return.
  useEffect(() => {
    const sub = AppState.addEventListener('change', state => {
      if (state === 'active') reconnectRef.current()
    })
    return () => sub.remove()
  }, [])

  // Container list is now pushed by lair on its persistent /stream — no HTTP poll.
  // The master ChatPane forwards `containers` events here via onContainersUpdate.
  const handleContainersUpdate = useCallback((list: ContainerInfo[]) => {
    log(`[app] agents push: ${list.length} agent(s)`)
    list.forEach(c => {
      log(`[app]   agent id=${c.id} name=${c.name} status=${c.status}`)
    })
    setContainers(list)
    const waitingId = startingContainerIdRef.current
    if (waitingId) {
      const started = list.find(c => c.id === waitingId && c.status === 'running')
      if (started) {
        log(`[app] agent ${started.name} is now running, switching chat to it`)
        startingContainerIdRef.current = null
        setStartingContainerId(null)
        setStartingError(null)
        setActiveChild(started)
      }
    }
  }, [])

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
    setShowSidebar(false)
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
    if (!masterSendFrameRef.current({ type: 'start_agent', id })) {
      const msg = 'master /stream not connected'
      logE(`[app] startContainer failed: ${msg}`)
      startingContainerIdRef.current = null
      setStartingContainerId(null)
      setStartingError(msg)
    }
    // No follow-up needed: lair will push a `containers` event when the
    // deployment scales, and handleContainersUpdate auto-connects to it.
  }, [masterBaseUrl])

  const terminateAgent = useCallback((c: ContainerInfo) => {
    if (!masterBaseUrl) return
    Vibration.vibrate(40)
    Alert.alert(
      'Terminate agent?',
      `This deletes "${containerDisplayName(c.name)}" and all its data. This cannot be undone.`,
      [
        { text: 'Cancel', style: 'cancel' },
        {
          text: 'Terminate',
          style: 'destructive',
          onPress: () => {
            log(`[app] terminateAgent id=${c.id}`)
            if (!masterSendFrameRef.current({ type: 'terminate_agent', id: c.id })) {
              logE('[app] terminateAgent failed: master /stream not connected')
            }
          },
        },
      ],
    )
  }, [masterBaseUrl])

  const openSidebar = useCallback(() => {
    // Containers are pushed live over /stream — no manual refresh needed.
    sidebarAnim.setValue(0)
    setShowSidebar(true)
    Animated.timing(sidebarAnim, { toValue: 1, duration: 240, useNativeDriver: true }).start()
  }, [sidebarAnim])

  const closeSidebar = useCallback(() => {
    Animated.timing(sidebarAnim, { toValue: 0, duration: 200, useNativeDriver: true }).start(({ finished }) => {
      if (finished) setShowSidebar(false)
    })
  }, [sidebarAnim])

  // ── QR scanner overlay ──────────────────────────────────────────────────────
  if (scanning) {
    return <QrScanner onScanned={handleQrScanned} onCancel={() => setScanning(false)} />
  }

  // ── Connection error screen ──────────────────────────────────────────────────
  if (conn && !tunnelPort && tunnelError) {
    return (
      <SafeAreaView style={s.setupSafe} edges={['top', 'bottom']}>
        <View style={s.setupCenter}>
          <AppIcon />
          <Text style={s.setupTitle}>OCTO</Text>
          <View style={s.setupRule} />
          <Text style={s.setupError}>{tunnelError}</Text>
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
          <TouchableOpacity onPress={requestCameraAndScan} activeOpacity={0.85}>
            <AppIcon pulse />
          </TouchableOpacity>
          <Text style={s.setupTitle}>OCTO</Text>
          <View style={s.setupRule} />
          <Text style={s.setupSubtitle}>Distributed Coding Agents</Text>
          <Text style={s.setupTagline}>Tap the mark to scan your session QR code.</Text>
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
        onClose={() => {
          // The Noise tunnel stays — only the in-app chat target changes.
          setActiveChild(null)
          setShowSidebar(false)
          sidebarAnim.setValue(0)
        }}
        initialDraft={draftsRef.current[childKey]}
        onDraftChange={d => { draftsRef.current[childKey] = d }}
        reconnectingRef={reconnectingRef}
        reloadRef={reloadRef}
        closeWsRef={closeWsRef}
      />
    )
  }

  // ── Master chat UI ───────────────────────────────────────────────────────────
  return (
    <SafeAreaView style={s.safe} edges={['top']}>
      <View style={s.paneArea}>
        <View style={s.header}>
          <View style={s.headerLeft}>
            <TouchableOpacity
              style={s.hamburgerBtn}
              onPress={openSidebar}
              hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
            >
              <View style={s.hamburgerBars}>
                <View style={s.hamburgerBar} />
                <View style={s.hamburgerBar} />
                <View style={s.hamburgerBar} />
              </View>
            </TouchableOpacity>
            <View style={[s.connStatusPill, { backgroundColor: statusColor(chatStatus) + '22' }]}>
              <View style={[s.connDot, { backgroundColor: statusColor(chatStatus) }]} />
              <Text style={[s.connPillLabel, { color: statusColor(chatStatus) }]}>{chatStatus === 'ready' || chatStatus === 'streaming' ? 'live' : chatStatus}</Text>
            </View>
          </View>
          <View style={s.headerRight}>
            <TasksHeaderButton tasks={masterTasks} onPress={() => setShowTasksModal(true)} />
            <TouchableOpacity
              style={s.clearBtn}
              onPress={() => clearChatRef.current()}
              hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
              disabled={chatStatus !== 'ready'}
            >
              <Text style={[s.clearBtnText, chatStatus !== 'ready' && { opacity: 0.3 }]}>clear</Text>
            </TouchableOpacity>
          </View>
        </View>

        {masterBaseUrl && (
          <ChatPane
            baseUrl={masterBaseUrl}
            onStatusChange={setChatStatus}
            clearRef={clearChatRef}
            initialDraft={draftsRef.current['master']}
            onDraftChange={d => { draftsRef.current['master'] = d }}
            reconnectingRef={reconnectingRef}
            reloadRef={reloadRef}
            closeWsRef={closeWsRef}
            sendFrameRef={masterSendFrameRef}
            onContainersUpdate={handleContainersUpdate}
            onTasksUpdate={setMasterTasks}
          />
        )}

        <TasksModal
          visible={showTasksModal}
          tasks={masterTasks}
          onClose={() => setShowTasksModal(false)}
          onCancel={(id) => {
            log(`[app] cancel_task id=${id} (master)`)
            masterSendFrameRef.current({ type: 'cancel_task', id })
          }}
        />

        {reconnecting && (
          <View style={s.startingOverlay} pointerEvents="none">
            <ActivityIndicator color={C.accent} size="large" />
            <Text style={s.startingText}>Connecting...</Text>
          </View>
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
        {showSidebar && (
          <>
            <Animated.View
              style={[StyleSheet.absoluteFillObject, s.sidebarBackdrop, { opacity: sidebarAnim.interpolate({ inputRange: [0, 1], outputRange: [0, 1] }) }]}
              pointerEvents="box-none"
            >
              <TouchableOpacity
                style={StyleSheet.absoluteFillObject}
                activeOpacity={1}
                onPress={closeSidebar}
              />
            </Animated.View>
            <Animated.View style={[s.sidebar, { transform: [{ translateX: sidebarAnim.interpolate({ inputRange: [0, 1], outputRange: [-280, 0] }) }] }]}>
              <View style={s.sidebarHeader}>
                <View>
                  <Text style={s.sidebarBrand}>OCTO</Text>
                  <Text style={s.sidebarBrandSub}>Agent Console</Text>
                </View>
                <TouchableOpacity onPress={closeSidebar} hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}>
                  <Text style={s.sidebarCloseIcon}>✕</Text>
                </TouchableOpacity>
              </View>
              <View style={s.sidebarSection}>
                <Text style={s.settingsMenuSectionTitle}>Repositories</Text>
              </View>
              <ScrollView
                style={{ flex: 1 }}
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
                      setShowSidebar(false)
                      sidebarAnim.setValue(0)
                      if (c.status === 'running') {
                        setActiveChild(c)
                      } else {
                        startContainer(c.id)
                      }
                    }}
                    onLongPress={() => terminateAgent(c)}
                    delayLongPress={500}
                    activeOpacity={0.7}
                  >
                    <View style={[s.containerDot, { backgroundColor: c.status === 'running' ? C.green : C.textMuted }]} />
                    <View style={{ flex: 1 }}>
                      <Text style={s.containerMenuItemName}>{containerDisplayName(c.name)}</Text>
                      {c.kind === 'remote' ? <Text style={s.containerMenuItemUrl} numberOfLines={1}>remote</Text> : null}
                    </View>
                    <Text style={s.containerMenuItemStatus}>{c.status}</Text>
                  </TouchableOpacity>
                ))}
              </ScrollView>
              <View style={s.settingsMenuDivider} />
              <TouchableOpacity style={s.sidebarExitBtn} onPress={handleLogout}>
                <Text style={s.settingsMenuLogoutText}>exit</Text>
              </TouchableOpacity>
            </Animated.View>
          </>
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
  // Setup / connection
  setupSafe:     { flex: 1, backgroundColor: C.bg },
  setupCenter:   { flex: 1, alignItems: 'center', justifyContent: 'center', paddingHorizontal: 40, gap: 12 },
  setupTitle:    { fontSize: 38, fontWeight: '800', color: C.textPrimary, letterSpacing: 9, fontFamily: NUNITO, marginTop: 18 },
  setupSubtitle: { fontSize: 10, color: C.textSecondary, letterSpacing: 3.5, textTransform: 'uppercase', fontFamily: MONO, fontWeight: '700' },
  setupRule:     { width: 28, height: 1, backgroundColor: C.textPrimary, marginVertical: 2, opacity: 0.4 },
  setupTagline:  { fontSize: 13, color: C.textSecondary, textAlign: 'center', lineHeight: 21, fontFamily: ARIMO, fontStyle: 'italic', marginTop: 14, maxWidth: 280 },
  setupDesc:     { fontSize: 14, color: C.textSecondary, textAlign: 'center', lineHeight: 22, fontFamily: ARIMO },
  setupStatus:   { fontSize: 11, color: C.textMuted, textAlign: 'center', fontFamily: MONO, letterSpacing: 1.5, textTransform: 'uppercase' },
  setupError:    { fontSize: 12, color: C.red, textAlign: 'center', lineHeight: 18, fontFamily: MONO, letterSpacing: 0.4 },
  setupBtn:      { borderRadius: 0, borderWidth: 1.5, borderColor: C.textPrimary, paddingVertical: 13, paddingHorizontal: 36, marginTop: 14, backgroundColor: 'transparent' },
  setupBtnText:  { color: C.textPrimary, fontWeight: '800', fontSize: 11, letterSpacing: 4, textTransform: 'uppercase', fontFamily: ARIMO },

  // App icon mark
  creatureImg:        { width: 116, height: 116, borderRadius: 26, marginBottom: 8 },

  // Inline transition overlays (starting / reconnecting)
  startingOverlay:    { ...StyleSheet.absoluteFillObject, backgroundColor: C.bg, alignItems: 'center', justifyContent: 'center', gap: 18, paddingHorizontal: 32 },
  startingText:       { fontSize: 11, color: C.textSecondary, fontFamily: MONO, letterSpacing: 2.5, textTransform: 'uppercase', fontWeight: '700' },
  startingErrorText:  { fontSize: 12, fontWeight: '800', color: C.red, fontFamily: ARIMO, textAlign: 'center', letterSpacing: 3, textTransform: 'uppercase' },
  startingErrorDetail:{ fontSize: 12, color: C.textSecondary, fontFamily: MONO, textAlign: 'center', lineHeight: 18 },
  startingCancelBtn:  { marginTop: 10, paddingVertical: 11, paddingHorizontal: 28, borderRadius: 0, borderWidth: 1.5, borderColor: C.textPrimary },
  startingCancelText: { fontSize: 11, color: C.textPrimary, fontFamily: ARIMO, letterSpacing: 4, textTransform: 'uppercase', fontWeight: '800' },

  // QR scanner
  scannerFull:       { ...StyleSheet.absoluteFillObject, backgroundColor: '#0A0E12', zIndex: 100 },
  scannerOverlay:    { ...StyleSheet.absoluteFillObject, alignItems: 'center', justifyContent: 'space-between', paddingVertical: 80 },
  scannerTopBar:     { alignItems: 'center', gap: 6, paddingHorizontal: 32 },
  scannerIcon:       { width: 56, height: 56, borderRadius: 13, marginBottom: 6 },
  scannerTitle:      { color: '#F4EFE3', fontSize: 12, fontWeight: '800', fontFamily: ARIMO, letterSpacing: 4, textTransform: 'uppercase' },
  scannerSubtitle:   { color: 'rgba(244,239,227,0.55)', fontSize: 11, textAlign: 'center', lineHeight: 17, fontFamily: MONO, letterSpacing: 1, marginTop: 4 },
  scannerReticle:    { width: 240, height: 240 },
  scannerCorner:     { position: 'absolute', width: 32, height: 32, borderColor: '#F4EFE3', borderWidth: 1.5 },
  cornerTL:          { top: 0, left: 0, borderRightWidth: 0, borderBottomWidth: 0 },
  cornerTR:          { top: 0, right: 0, borderLeftWidth: 0, borderBottomWidth: 0 },
  cornerBL:          { bottom: 0, left: 0, borderRightWidth: 0, borderTopWidth: 0 },
  cornerBR:          { bottom: 0, right: 0, borderLeftWidth: 0, borderTopWidth: 0 },
  scannerCancel:     { borderWidth: 1, borderColor: 'rgba(244,239,227,0.45)', borderRadius: 0, paddingVertical: 12, paddingHorizontal: 36 },
  scannerCancelText: { color: '#F4EFE3', fontSize: 11, fontWeight: '800', fontFamily: ARIMO, letterSpacing: 4, textTransform: 'uppercase' },
  scannerError:      { color: '#E07057', fontSize: 13, textAlign: 'center', marginBottom: 24, fontFamily: MONO, letterSpacing: 1 },

  // Chat layout
  safe:         { flex: 1, backgroundColor: C.bg },
  paneArea:     { flex: 1 },

  // Header
  header:          { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: 14, paddingVertical: 12, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border, backgroundColor: C.bg },
  headerLeft:      { flexDirection: 'row', alignItems: 'center', gap: 10, flex: 1 },
  backBtn:         { paddingRight: 6, paddingVertical: 2 },
  backBtnText:     { fontSize: 30, lineHeight: 32, color: C.textPrimary, fontWeight: '300', fontFamily: ARIMO },
  clearBtn:        { paddingVertical: 5, paddingHorizontal: 10, borderWidth: StyleSheet.hairlineWidth, borderColor: C.border, borderRadius: 0 },
  clearBtnText:    { fontSize: 10, color: C.textSecondary, fontWeight: '800', fontFamily: ARIMO, letterSpacing: 2.2, textTransform: 'uppercase' },
  headerTitle:     { fontSize: 12, fontWeight: '800', color: C.textPrimary, fontFamily: ARIMO, letterSpacing: 2, textTransform: 'uppercase' },
  // Status as a typographic tag — small square index, monospace label, hairline border
  connDot:         { width: 6, height: 6, borderRadius: 0 },
  connStatusPill:  { flexDirection: 'row', alignItems: 'center', justifyContent: 'center', paddingHorizontal: 8, paddingVertical: 4, borderRadius: 0, gap: 6, borderWidth: StyleSheet.hairlineWidth, borderColor: C.border },
  connPillLabel:   { fontSize: 10, fontWeight: '800', fontFamily: MONO, letterSpacing: 1.6, textTransform: 'uppercase' },

  // Chat pane
  pane:               { flex: 1, backgroundColor: C.bg },
  messageList:        { flex: 1 },
  messageListContent: { paddingVertical: 18 },
  emptyStateWrap:     { alignItems: 'center', marginTop: 88, gap: 6 },
  emptyStateBrand:    { fontSize: 30, color: C.textPrimary, fontWeight: '800', letterSpacing: 8, fontFamily: NUNITO, marginTop: 8 },
  emptyStateRule:     { width: 24, height: 1, backgroundColor: C.textPrimary, opacity: 0.35, marginTop: 4 },
  emptyStateTagline:  { fontSize: 10, color: C.textMuted, fontFamily: MONO, letterSpacing: 2.6, textTransform: 'uppercase', marginTop: 6, fontWeight: '700' },
  reconnectBanner:    { position: 'absolute', top: 0, left: 0, right: 0, flexDirection: 'row', alignItems: 'center', justifyContent: 'center', paddingVertical: 6, borderBottomWidth: StyleSheet.hairlineWidth, zIndex: 10 },
  reconnectText:      { fontSize: 10, fontWeight: '800', fontFamily: MONO, letterSpacing: 2, textTransform: 'uppercase' },

  // Scroll-to-bottom — sharp-cornered tile rather than a floating circle
  scrollBtnWrap:     { position: 'absolute', left: 0, right: 0, alignItems: 'center', pointerEvents: 'box-none' },
  scrollBtn:         { backgroundColor: C.bg, borderRadius: 0, width: 32, height: 32, alignItems: 'center', justifyContent: 'center', borderWidth: 1, borderColor: C.textPrimary, marginBottom: 10 },
  scrollBtnIcon:     { fontSize: 14, color: C.textPrimary, lineHeight: 16, fontFamily: ARIMO, fontWeight: '700' },

  // Messages
  messageWrap:         { paddingHorizontal: 16, marginBottom: 14 },
  messageWrapRight:    { alignItems: 'flex-end' },
  // User bubble: icon-blue, white text — ties the user voice to the app mark
  userBubble:          { backgroundColor: '#4A90E2', borderRadius: 18, borderBottomRightRadius: 4, paddingHorizontal: 14, paddingVertical: 10, maxWidth: '82%' },
  textBlock:           { color: '#FFFFFF', fontSize: 15.5, lineHeight: 23, fontWeight: '400', fontFamily: ARIMO },
  assistantTextBlock:  { color: '#1F2937', fontSize: 15.5, lineHeight: 24, fontWeight: '400', fontFamily: ARIMO },
  inlineCode:          { fontFamily: MONO, fontSize: 12.5, color: C.textPrimary, backgroundColor: C.surface, paddingHorizontal: 4, paddingVertical: 1, borderRadius: 2, borderWidth: StyleSheet.hairlineWidth, borderColor: C.border },
  codeBlock:           { backgroundColor: C.surface, borderRadius: 4, padding: 12, marginVertical: 6, borderWidth: StyleSheet.hairlineWidth, borderColor: C.border },
  codeBlockText:       { fontFamily: MONO, fontSize: 12, color: C.textPrimary, lineHeight: 18 },
  codeBlockLang:       { fontSize: 9, color: C.textMuted, fontFamily: MONO, marginBottom: 8, textTransform: 'uppercase', letterSpacing: 1.6, fontWeight: '700' },
  questionMark:        { color: C.yellow, fontWeight: '700', fontSize: 15, marginBottom: 2, fontFamily: ARIMO },
  costLabel:           { fontSize: 10, color: C.textMuted, marginTop: 6, marginLeft: 2, fontFamily: MONO, letterSpacing: 0.8 },
  // Tool chip — terminal-style, sharp corners, monospace, accent-tinted surface
  toolChip:            { backgroundColor: C.accentLight, borderLeftWidth: 2, borderLeftColor: C.accent, borderRadius: 0, paddingHorizontal: 12, paddingVertical: 8 },
  toolLine:            { fontSize: 12.5, color: C.accent, fontFamily: MONO, letterSpacing: 0.3 },
  toolChevron:         { fontSize: 14, color: C.accent, marginLeft: 6, fontWeight: '400' },
  toolOutputBlock:     { marginTop: 8, paddingTop: 8, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: C.border },
  toolOutputText:      { fontSize: 11.5, color: C.textSecondary, fontFamily: MONO, lineHeight: 17 },
  interruptedLine:     { fontSize: 11, lineHeight: 18, color: C.textMuted, fontFamily: MONO, letterSpacing: 2.2, textTransform: 'uppercase', fontWeight: '700' },
  bgCompleteLine:      { fontSize: 12, lineHeight: 18, color: C.textMuted, fontFamily: MONO, letterSpacing: 0.5, fontStyle: 'italic' },
  errorLine:           { fontSize: 12, lineHeight: 18, color: C.red, fontFamily: MONO, letterSpacing: 0.6, fontWeight: '700' },

  // Input bar
  completionList:  { position: 'absolute', left: 0, right: 0, maxHeight: 180, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: C.border, backgroundColor: C.bg, zIndex: 10, elevation: 10 },
  completionItem:  { paddingHorizontal: 16, paddingVertical: 10, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  completionText:  { fontSize: 13, color: C.textPrimary, fontFamily: MONO },
  inputFloat:      { position: 'absolute', bottom: 0, left: 0, right: 0, paddingHorizontal: 12, paddingBottom: 10, paddingTop: 10, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: C.border, backgroundColor: C.bg },
  inputRow:        { flexDirection: 'row', alignItems: 'flex-end', gap: 8 },
  // Input — slightly raised paper tone with a sharper corner radius
  input:           { flex: 1, backgroundColor: '#FBF8EE', borderWidth: 1, borderColor: C.border, borderRadius: 6, paddingHorizontal: 16, paddingVertical: 15, color: C.textPrimary, fontSize: 15.5, lineHeight: 26, minHeight: 56, maxHeight: 140, fontFamily: ARIMO },
  // Send button is squared to match the input box; bg matches the app icon
  sendBtn:         { width: 56, height: 56, borderRadius: 6, backgroundColor: '#4A90E2', alignItems: 'center', justifyContent: 'center', marginBottom: 0 },
  sendBtnDisabled: { backgroundColor: C.surface, borderWidth: 1, borderColor: C.border },
  sendBtnIcon:     { fontSize: 22, color: '#FFFFFF', fontWeight: '700', lineHeight: 24, fontFamily: ARIMO },
  // Paper-plane send icon — built from two border-triangles. See PaperPlane
  // component for the geometry.
  paperPlaneWrap:  { width: 24, height: 24, alignItems: 'center', justifyContent: 'center' },
  paperPlaneTilt:  { transform: [{ rotate: '-22deg' }], marginLeft: 1 },
  paperPlaneWing:  { width: 0, height: 0, borderTopWidth: 8, borderBottomWidth: 8, borderLeftWidth: 19, borderTopColor: 'transparent', borderBottomColor: 'transparent' },
  paperPlaneNotch: { position: 'absolute', top: 8, left: 0, width: 0, height: 0, borderTopWidth: 5, borderLeftWidth: 11, borderTopColor: 'transparent' },
  // Streaming-state stop button — same footprint as sendBtn so the layout
  // doesn't shift between idle and streaming. An OrbitingArc sits behind
  // the stop button and circles around its perimeter.
  inputBtnSlot:       { width: 56, height: 56, marginBottom: 0, alignItems: 'center', justifyContent: 'center' },
  // Only the top edge is colored; the others are transparent. With a circular
  // borderRadius this renders as a ~90° arc, which appears to travel around
  // the button's perimeter when the View itself is rotated.
  orbitArc:           { position: 'absolute', borderColor: 'transparent', borderTopColor: C.accent },
  stopBtnInline:      { width: 50, height: 50, borderRadius: 25, backgroundColor: '#FF3B30', alignItems: 'center', justifyContent: 'center' },
  stopBtnInlineIcon:  { fontSize: 18, color: '#FFFFFF', fontWeight: '700', lineHeight: 20 },

  // Header right buttons
  headerRight:              { flexDirection: 'row', alignItems: 'center', gap: 8 },
  // Tasks header button — typographic tag with a small status dot. Mirrors the
  // monospace + hairline-border treatment used by clearBtn / connStatusPill.
  tasksBtn:                 { flexDirection: 'row', alignItems: 'center', gap: 6, paddingVertical: 5, paddingHorizontal: 10, borderWidth: StyleSheet.hairlineWidth, borderColor: C.border, borderRadius: 0 },
  tasksBtnDot:              { width: 6, height: 6, borderRadius: 0 },
  tasksBtnText:             { fontSize: 10, color: C.textSecondary, fontWeight: '800', fontFamily: MONO, letterSpacing: 1.6 },

  // Tasks slide-up modal
  tasksBackdrop:            { ...StyleSheet.absoluteFillObject, backgroundColor: 'rgba(14,26,36,0.42)', zIndex: 300 },
  tasksSheet:               { position: 'absolute', left: 0, right: 0, bottom: 0, maxHeight: '78%', backgroundColor: C.bg, zIndex: 301, borderTopWidth: 1, borderTopColor: C.border, paddingTop: 8, shadowColor: '#000', shadowOpacity: 0.15, shadowRadius: 20, shadowOffset: { width: 0, height: -4 }, elevation: 18 },
  tasksHandle:              { alignSelf: 'center', width: 44, height: 4, borderRadius: 2, backgroundColor: C.border, marginBottom: 8 },
  tasksHeader:              { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: 18, paddingVertical: 12, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  tasksHeaderTitle:         { fontSize: 13, fontWeight: '800', color: C.textPrimary, fontFamily: ARIMO, letterSpacing: 2.5, textTransform: 'uppercase' },
  tasksEmptyWrap:           { paddingVertical: 60, alignItems: 'center' },
  tasksEmptyText:           { fontSize: 11, color: C.textMuted, fontFamily: MONO, letterSpacing: 2.4, textTransform: 'uppercase', fontWeight: '700' },

  // A single task row inside the modal
  taskRow:                  { paddingHorizontal: 18, paddingVertical: 14, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  taskRowHeader:            { flexDirection: 'row', alignItems: 'center', gap: 10, marginBottom: 8 },
  taskStatusTag:            { flexDirection: 'row', alignItems: 'center', gap: 5, paddingHorizontal: 6, paddingVertical: 3, borderWidth: StyleSheet.hairlineWidth },
  taskStatusDot:            { width: 5, height: 5, borderRadius: 0 },
  taskStatusLabel:          { fontSize: 9, fontWeight: '800', fontFamily: MONO, letterSpacing: 1.6 },
  taskTimestamp:            { fontSize: 10, color: C.textMuted, fontFamily: MONO, letterSpacing: 1, flex: 1 },
  taskStopBtn:              { borderWidth: 1, borderColor: C.red, paddingVertical: 4, paddingHorizontal: 10, borderRadius: 0 },
  taskStopText:             { fontSize: 10, color: C.red, fontWeight: '800', fontFamily: ARIMO, letterSpacing: 2.2, textTransform: 'uppercase' },
  taskDescription:          { fontSize: 14, color: C.textPrimary, fontFamily: ARIMO, lineHeight: 20 },
  taskSummary:              { fontSize: 12, color: C.textSecondary, fontFamily: MONO, lineHeight: 17, marginTop: 6 },
  taskCost:                 { fontSize: 10, color: C.textMuted, fontFamily: MONO, marginTop: 6, letterSpacing: 0.6 },
  // Hamburger as three deliberate bars (replaces the typographic glyph)
  hamburgerBtn:             { paddingVertical: 8, paddingHorizontal: 6, marginRight: 4 },
  hamburgerBars:            { width: 18, height: 12, justifyContent: 'space-between' },
  hamburgerBar:             { height: 1.5, backgroundColor: C.textPrimary },
  hamburgerBtnText:         { fontSize: 18, color: C.textPrimary, fontFamily: ARIMO, fontWeight: '700' },
  containerDot:             { width: 6, height: 6, borderRadius: 0 },

  // Sidebar — editorial drawer
  sidebarBackdrop:          { backgroundColor: 'rgba(14,26,36,0.32)', zIndex: 200 },
  sidebar:                  { position: 'absolute', top: 0, left: 0, bottom: 0, width: 300, backgroundColor: C.bg, zIndex: 201, borderRightWidth: 1, borderRightColor: C.border, shadowColor: '#000', shadowOpacity: 0.12, shadowRadius: 20, shadowOffset: { width: 4, height: 0 }, elevation: 16, flexDirection: 'column' },
  sidebarSection:           { paddingHorizontal: 18, paddingTop: 20, paddingBottom: 8 },
  sidebarHeader:            { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: 18, paddingVertical: 18, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  sidebarBrand:             { fontSize: 22, fontWeight: '800', color: C.textPrimary, letterSpacing: 6, fontFamily: NUNITO },
  sidebarBrandSub:          { fontSize: 9, fontWeight: '800', color: C.textMuted, letterSpacing: 2.5, fontFamily: MONO, textTransform: 'uppercase', marginTop: 4 },
  sidebarCloseIcon:         { fontSize: 16, color: C.textSecondary, fontFamily: ARIMO, fontWeight: '300' },
  sidebarExitBtn:           { paddingHorizontal: 18, paddingVertical: 16 },
  settingsMenuSectionTitle: { fontSize: 10, fontWeight: '800', color: C.textMuted, textTransform: 'uppercase', letterSpacing: 2.5, fontFamily: MONO },
  settingsMenuDivider:      { height: StyleSheet.hairlineWidth, backgroundColor: C.border },
  settingsMenuLogoutText:   { fontSize: 11, color: C.red, fontFamily: ARIMO, fontWeight: '800', letterSpacing: 2.8, textTransform: 'uppercase' },
  containerMenuItem:        { flexDirection: 'row', alignItems: 'center', gap: 12, paddingHorizontal: 18, paddingVertical: 14, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  containerMenuItemName:    { fontSize: 14, fontWeight: '700', color: C.textPrimary, fontFamily: ARIMO, letterSpacing: 0.3 },
  containerMenuItemUrl:     { fontSize: 11, color: C.textMuted, fontFamily: MONO, marginTop: 3, letterSpacing: 0.3 },
  containerMenuItemStatus:  { fontSize: 9, color: C.textMuted, fontFamily: MONO, letterSpacing: 1.6, textTransform: 'uppercase', fontWeight: '800' },
})
