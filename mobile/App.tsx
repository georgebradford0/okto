import AsyncStorage from '@react-native-async-storage/async-storage'
import React, { useCallback, useEffect, useId, memo, useMemo, useRef, useState } from 'react'
import {
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
  Text as RNText,
  TextInput,
  useWindowDimensions,
  Vibration,
  View as RNView,
} from 'react-native'
import { KeyboardProvider, useReanimatedKeyboardAnimation } from 'react-native-keyboard-controller'
import Reanimated, { useAnimatedStyle } from 'react-native-reanimated'
import Svg, { Defs, LinearGradient as SvgLinearGradient, Rect, Stop } from 'react-native-svg'
import { SafeAreaProvider, SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context'
import { Camera, useCameraDevice, useCodeScanner } from 'react-native-vision-camera'
import NoiseConnection from './src/NativeNoiseConnection'
import {
  loadHistory as loadCachedHistory,
  saveHistory as saveCachedHistory,
  clearHistory as clearCachedHistory,
  clearAllHistory as clearAllCachedHistory,
} from './src/historyCache'
import { registerWithRelay } from './src/registerWithRelay'
import {
  type ClientFrame,
  type AgentInfo as WireAgentInfo,
  type ServerEvent,
  type TaskRecord,
  type WorktreeMeta,
  WIRE_PROTOCOL,
  encodeClientFrame,
  parseServerEvent,
} from './src/wire'
import {
  OktoProvider,
  Spinner,
  Button,
  ButtonText,
  Touchable,
  View,
  Text,
} from '@okto/ui'
import { Send } from 'lucide-react-native'

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
  role:       'user' | 'assistant' | 'tool' | 'session' | 'interrupted' | 'error' | 'bg_complete' | 'bg_progress'
  text:       string
  cost?:      number
  output?:    string
  running?:   boolean
  prevRole?:  Message['role']
}

// A FlatList row: either a single message bubble, or a coalesced run of
// consecutive tool calls rendered as one collapsible ToolGroup.
type RenderRow =
  | { kind: 'msg';   key: string; message: Message }
  | { kind: 'group'; key: string; tools: Message[] }

// ── Logging ────────────────────────────────────────────────────────────────────

const ts = () => new Date().toISOString().replace('T', ' ').slice(0, 23)
const log  = (...args: unknown[]) => console.log( `[${ts()}]`, ...args)
const logE = (...args: unknown[]) => console.error(`[${ts()}] ERROR`, ...args)

// ── Helpers ────────────────────────────────────────────────────────────────────

let _id = 0
const uid = () => `m${Date.now()}_${++_id}`

/** Delay between consecutive history-replay appends on first load. Tuned so
 *  each MessageBubble's 180ms fade-in overlaps with the next bubble starting
 *  to fade — fast enough that long histories finish quickly, slow enough
 *  that a viewer perceives motion rather than a flicker. */
const HISTORY_STAGGER_MS = 35

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
// Warm-paper aesthetic, tuned for a modern mobile feel: a lifted cream canvas
// with white elevated surfaces, a refined oceanic teal accent, and a generous
// neutral type ramp. Identity stays (cream + teal + ink), but tones are
// softer and corners are rounded throughout the StyleSheet below.

// Palette aligned with the shared @okto/ui design tokens (gluestack v3 theme):
// modern-neutral surfaces, a teal brand accent, and a cool slate type ramp.
// Mirrors the desktop theme so both clients read as one product.
const C = {
  bg:            '#F8FAFC',  // neutral near-white canvas (slate-50)
  bgElevated:    '#FFFFFF',  // cards, sheets, raised surfaces
  surface:       '#F1F5F9',  // tonal hover / chip background (slate-100)
  surfaceSoft:   '#F8FAFC',  // lifted surface for inputs
  border:        '#E2E8F0',  // hairline divider (slate-200)
  borderStrong:  '#CBD5E1',  // emphasis border (slate-300)
  accent:        '#0D9488',  // brand teal — "live" / accents (primary-600)
  accentStrong:  '#0F766E',  // pressed / strong accent (teal-700)
  accentLight:   '#CCFBF1',  // teal-tinted (active rows, chips) (primary-50)
  green:         '#15803D',  // success / "done"
  greenLight:    '#DCFCE7',
  yellow:        '#B45309',  // warning / pending (amber-700)
  yellowLight:   '#FEF3C7',
  red:           '#DC2626',  // error / destructive (error-600)
  redLight:      '#FEE2E2',
  userBlue:      '#0D9488',  // user bubble + send button — brand teal
  textPrimary:   '#0F172A',  // slate-900
  textSecondary: '#475569',  // slate-600
  textMuted:     '#64748B',  // slate-500 — placeholders / status meta
  textFaint:     '#94A3B8',  // slate-400 — captions / hint level
  inputBorder:   '#E2E8F0',  // slate-200
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

// Tool label rendered as plain monospace text (no chip). While the tool runs, a
// soft light band sweeps across the glyphs — a "shimmer" that signals live
// output the user can expand to watch, replacing the old pulsing dot. Queued
// tools render dimmed; finished tools render plain. The label is always a single
// <Text> node so it stays selectable and queryable.
function ShimmerText({
  text, running, dim, numberOfLines,
}: {
  text:          string
  running:       boolean
  dim:           boolean
  numberOfLines?: number
}) {
  const [size, setSize] = useState({ w: 0, h: 0 })
  const sweep = useRef(new Animated.Value(0)).current
  // Unique gradient id per instance so concurrent shimmers don't collide.
  const gradId = `shimmer-${useId().replace(/[^a-zA-Z0-9]/g, '')}`

  useEffect(() => {
    if (!running || size.w === 0) return
    sweep.setValue(0)
    const loop = Animated.loop(
      Animated.timing(sweep, {
        toValue: 1,
        duration: 1500,
        easing: Easing.inOut(Easing.ease),
        useNativeDriver: true,
      }),
    )
    loop.start()
    return () => loop.stop()
  }, [running, size.w, sweep])

  const band = Math.max(48, size.w * 0.45)
  const translateX = sweep.interpolate({
    inputRange:  [0, 1],
    outputRange: [-band, size.w + band],
  })

  return (
    <View style={{ alignSelf: 'flex-start', maxWidth: '100%', overflow: 'hidden' }}>
      <Text
        onLayout={e => setSize({ w: e.nativeEvent.layout.width, h: e.nativeEvent.layout.height })}
        fontSize={13} fontWeight="600" letterSpacing={0.2} color="$primary700"
        style={{ fontFamily: MONO, opacity: dim ? 0.45 : 1 }}
        selectable numberOfLines={numberOfLines} ellipsizeMode="tail"
      >
        {text}
      </Text>
      {running && size.w > 0 && (
        <Animated.View
          pointerEvents="none"
          style={{ position: 'absolute', top: 0, bottom: 0, left: 0, width: band, transform: [{ translateX }] }}
        >
          <Svg width={band} height={size.h || 20}>
            <Defs>
              <SvgLinearGradient id={gradId} x1="0" y1="0" x2="1" y2="0">
                <Stop offset="0" stopColor="#ffffff" stopOpacity={0} />
                <Stop offset="0.5" stopColor="#ffffff" stopOpacity={0.7} />
                <Stop offset="1" stopColor="#ffffff" stopOpacity={0} />
              </SvgLinearGradient>
            </Defs>
            <Rect x="0" y="0" width={band} height={size.h || 20} fill={`url(#${gradId})`} />
          </Svg>
        </Animated.View>
      )}
    </View>
  )
}

// A single tool row: a shimmering label (live while running, dimmed while
// queued) that taps to reveal the tool's streamed/finished output. Shared by
// the standalone single-tool bubble and the expanded ToolGroup list.
const ToolRow = memo(function ToolRow({
  message, paddingHorizontal = 16,
}: {
  message:           Message
  paddingHorizontal?: number
}) {
  const [expanded, setExpanded] = useState(false)
  const running = !!message.running
  const queued  = !running && message.output === undefined
  return (
    <Touchable
      marginBottom={4} paddingHorizontal={paddingHorizontal}
      onPress={() => setExpanded(v => !v)}
      activeOpacity={0.6}
    >
      <ShimmerText
        text={message.text}
        running={running}
        dim={queued}
        numberOfLines={expanded ? undefined : 1}
      />
      {expanded && message.output != null && (
        <View marginTop={6} paddingLeft={2}>
          <ScrollView style={{ maxHeight: 180 }} nestedScrollEnabled showsVerticalScrollIndicator={false}>
            <Text fontSize={12} lineHeight={18} color="$typography600" style={{ fontFamily: MONO }} selectable>{message.output}</Text>
          </ScrollView>
        </View>
      )}
    </Touchable>
  )
})

// A run of consecutive tool calls, collapsed into one block so a long agentic
// loop doesn't flood the transcript. Collapsed: while a step is running we show
// just that step's shimmer plus an `n/total` progress counter; once idle we show
// a muted "N tool calls" summary. Tap to expand the full list of ToolRows (each
// independently expandable for its output). A lone tool renders inline (no
// group chrome) — see the renderRows grouping in ChatPane.
const ToolGroup = memo(function ToolGroup({
  tools,
}: {
  tools: Message[]
}) {
  const [open, setOpen] = useState(false)
  const fadeAnim = useRef(new Animated.Value(0)).current
  useEffect(() => {
    Animated.timing(fadeAnim, { toValue: 1, duration: 180, useNativeDriver: true }).start()
  }, [])

  const total      = tools.length
  const active     = tools.find(t => t.running)
  const doneCount  = tools.filter(t => t.output !== undefined).length

  return (
    <Animated.View style={{ opacity: fadeAnim }}>
      <Touchable
        paddingHorizontal={16}
        marginBottom={open ? 6 : 4}
        onPress={() => setOpen(v => !v)}
        activeOpacity={0.6}
      >
        {active && !open ? (
          // Live + collapsed: surface the running step, hide the rest.
          <View flexDirection="row" alignItems="center" gap={8}>
            <View flex={1}>
              <ShimmerText text={active.text} running dim={false} numberOfLines={1} />
            </View>
            <Text fontSize={11} color="$typography400" style={{ fontFamily: MONO }}>{Math.min(doneCount + 1, total)}/{total}</Text>
          </View>
        ) : (
          <Text fontSize={13} color="$typography500" style={{ fontFamily: MONO }}>
            {open ? '▾' : '▸'} {total} tool calls
          </Text>
        )}
      </Touchable>
      {open && (
        <View marginBottom={4}>
          {tools.map(t => <ToolRow key={t.id} message={t} paddingHorizontal={28} />)}
        </View>
      )}
    </Animated.View>
  )
})

const MessageBubble = memo(function MessageBubble({
  message, prevRole,
}: {
  message:   Message
  prevRole?: Message['role']
}) {
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
        <View paddingHorizontal={16} paddingLeft={28} style={{ marginBottom: bubbleBottomMargin }}>
          <Text selectable alignSelf="flex-start" overflow="hidden" borderRadius={10} backgroundColor="$error50" paddingHorizontal={12} paddingVertical={8} fontFamily="$body" fontSize={13} fontWeight="500" lineHeight={19} color="$error600">⚠ {message.text}</Text>
        </View>
      </Animated.View>
    )
  }
  if (message.role === 'interrupted') {
    return (
      <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
        <View paddingHorizontal={16} paddingLeft={28} style={{ marginBottom: bubbleBottomMargin }}>
          <Text selectable fontSize={11} fontWeight="700" textTransform="uppercase" lineHeight={18} letterSpacing={1.6} color="$typography500" style={{ fontFamily: MONO }}>■ interrupted</Text>
        </View>
      </Animated.View>
    )
  }
  if (message.role === 'bg_complete' || message.role === 'bg_progress') {
    // Take just the first line of the injected text — it's prefixed with a
    // "Background command <id> completed…" / "[monitor] … produced new output:"
    // header which is enough context; the long body would crowd the chip.
    const firstLine = message.text.split('\n', 1)[0] || message.text
    const marker = message.role === 'bg_progress' ? '◈' : '◇'
    return (
      <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
        <View paddingHorizontal={16} paddingLeft={28} style={{ marginBottom: bubbleBottomMargin }}>
          <Text selectable fontFamily="$body" fontSize={12.5} fontStyle="italic" lineHeight={19} color="$typography500">{marker} {firstLine}</Text>
        </View>
      </Animated.View>
    )
  }
  if (message.role === 'tool') {
    return (
      <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
        <ToolRow message={message} />
      </Animated.View>
    )
  }
  if (message.role === 'user') {
    return (
      <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
        <View alignItems="flex-end" paddingHorizontal={16} style={{ marginBottom: bubbleBottomMargin }}>
          <View maxWidth="82%" borderRadius={22} borderBottomRightRadius={6} backgroundColor="$primary600" paddingHorizontal={15} paddingVertical={10}>
            {renderedText}
          </View>
        </View>
      </Animated.View>
    )
  }
  return (
    <Animated.View style={{ opacity: fadeAnim, marginTop: extraTopMargin }}>
      <View paddingHorizontal={16} style={{ marginBottom: bubbleBottomMargin }}>
        {renderedText}
        {message.cost != null && (
          <Text marginLeft={2} marginTop={6} fontSize={10.5} letterSpacing={0.4} color="$typography400" style={{ fontFamily: MONO }}>{formatCost(message.cost)}</Text>
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
    <Touchable
      flexDirection="row" alignItems="center" gap={7} borderRadius={999} borderWidth={1} borderColor="$outline200" backgroundColor="$background0" paddingHorizontal={12} paddingVertical={5}
      onPress={onPress}
      hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
    >
      <View height={7} width={7} borderRadius={999} backgroundColor={runningCount > 0 ? '$primary600' : '$typography500'} />
      <Text fontFamily="$body" fontSize={11} fontWeight="700" letterSpacing={0.4} color="$typography600">
        {runningCount > 0 ? `TASKS · ${runningCount}` : 'TASKS'}
      </Text>
    </Touchable>
  )
}

const TaskRow = memo(function TaskRow({ task, cancelling, onCancel }: { task: TaskRecord; cancelling: boolean; onCancel: () => void }) {
  const [expanded, setExpanded] = useState(false)
  const isRunning = task.status === 'running'
  const ts = task.completed_at != null ? relativeTime(task.completed_at) : relativeTime(task.started_at)
  const statusColor = taskStatusColor(task.status)
  return (
    <View borderBottomWidth={1} borderColor="$outline200" paddingHorizontal={20} paddingVertical={14}>
      <View marginBottom={8} flexDirection="row" alignItems="center" gap={10}>
        <View flexDirection="row" alignItems="center" gap={6} borderRadius={999} borderWidth={1} paddingHorizontal={9} paddingVertical={3} style={{ borderColor: statusColor }}>
          <View height={6} width={6} borderRadius={999} style={{ backgroundColor: statusColor }} />
          <Text fontSize={10} fontWeight="700" letterSpacing={1.2} style={{ color: statusColor, fontFamily: MONO }}>
            {task.status.toUpperCase()}
          </Text>
        </View>
        {task.wake_interval_secs != null && (
          <Text fontSize={10} fontWeight="700" letterSpacing={1.2} color="$primary600" style={{ fontFamily: MONO }}>◈ MONITORED</Text>
        )}
        <Text flex={1} fontSize={11} letterSpacing={0.4} color="$typography500" style={{ fontFamily: MONO }}>{ts}</Text>
        {isRunning && (
          <Touchable
            borderRadius={999} borderWidth={1} borderColor="$error600" paddingHorizontal={12} paddingVertical={4} opacity={cancelling ? 0.4 : undefined}
            onPress={onCancel}
            disabled={cancelling}
            hitSlop={{ top: 6, bottom: 6, left: 6, right: 6 }}
          >
            <Text fontFamily="$body" fontSize={11} fontWeight="700" letterSpacing={0.4} color="$error600">{cancelling ? 'STOPPING' : 'STOP'}</Text>
          </Touchable>
        )}
      </View>
      <Touchable activeOpacity={0.7} onPress={() => setExpanded(v => !v)}>
        <Text fontFamily="$body" fontSize={14.5} lineHeight={21} color="$typography900" numberOfLines={expanded ? undefined : 2} selectable>
          {task.command}
        </Text>
        {task.summary != null && task.summary.length > 0 && (
          <Text marginTop={6} fontFamily="$body" fontSize={13} lineHeight={19} color="$typography600" numberOfLines={expanded ? undefined : 2} selectable>
            {task.summary}
          </Text>
        )}
        {task.cost_usd != null && task.cost_usd > 0 && (
          <Text marginTop={8} fontSize={10.5} letterSpacing={0.3} color="$typography400" style={{ fontFamily: MONO }}>{formatCost(task.cost_usd)}</Text>
        )}
      </Touchable>
    </View>
  )
})

function TasksModal({ visible, tasks, cancellingIds, onClose, onCancel }: {
  visible:       boolean
  tasks:         TaskRecord[]
  cancellingIds: Set<string>
  onClose:       () => void
  onCancel:      (taskId: string) => void
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
        style={[StyleSheet.absoluteFill, { zIndex: 300, opacity: slide, backgroundColor: 'rgba(2,6,23,0.4)' }]}
        pointerEvents={visible ? 'auto' : 'none'}
      >
        <Touchable style={StyleSheet.absoluteFill} activeOpacity={1} onPress={onClose} />
      </Animated.View>
      <Animated.View
        style={{
          position: 'absolute', left: 0, right: 0, bottom: 0, zIndex: 301, maxHeight: '78%',
          borderTopLeftRadius: 22, borderTopRightRadius: 22, backgroundColor: '#fff', paddingTop: 10,
          paddingBottom: insets.bottom + 16,
          transform: [{ translateY: slide.interpolate({ inputRange: [0, 1], outputRange: [600, 0] }) }],
        }}
      >
        <View marginBottom={10} height={5} width={40} alignSelf="center" borderRadius={999} backgroundColor="$outline300" opacity={0.6} />
        <View flexDirection="row" alignItems="center" justifyContent="space-between" borderBottomWidth={1} borderColor="$outline200" paddingHorizontal={20} paddingVertical={14}>
          <Text fontFamily="$body" fontSize={17} fontWeight="700" color="$typography900">Background Tasks</Text>
          <Touchable onPress={onClose} hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}>
            <Text fontSize={16} fontWeight="300" color="$typography600">✕</Text>
          </Touchable>
        </View>
        <ScrollView
          style={{ flex: 1 }}
          contentContainerStyle={{ paddingBottom: 20 }}
          showsVerticalScrollIndicator={false}
        >
          {sorted.length === 0 ? (
            <View alignItems="center" paddingVertical={60}>
              <Text fontFamily="$body" fontSize={13} fontStyle="italic" color="$typography500">No background tasks</Text>
            </View>
          ) : sorted.map(t => (
            <TaskRow
              key={t.task_id}
              task={t}
              cancelling={cancellingIds.has(t.task_id)}
              onCancel={() => onCancel(t.task_id)}
            />
          ))}
        </ScrollView>
      </Animated.View>
    </View>
  )
}

// ── useCancelGuard ────────────────────────────────────────────────────────────

// Optimistic guard for the task STOP button. A press latches the task id into
// `cancellingIds` (button shows STOPPING, disabled) and sends one `cancel_task`
// frame. The latch is released by whichever lands first:
//   • `cancel_task_ack` with fired=false — server had nothing live to cancel.
//   • a `tasks` frame moving the task off `running` — the cancel took effect.
//   • CANCEL_ACK_TIMEOUT_MS with no ack — the frame was likely lost on a WS
//     hiccup; un-latch so the user can retry rather than latching forever.
// An ack with fired=true keeps the latch: the kill is in progress and the
// follow-up `tasks` frame will release it.
const CANCEL_ACK_TIMEOUT_MS = 6000

function useCancelGuard(sendFrameRef: React.MutableRefObject<(frame: ClientFrame) => boolean>) {
  const [cancellingIds, setCancellingIds] = useState<Set<string>>(() => new Set())
  const timersRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map())

  const clearTimer = useCallback((id: string) => {
    const t = timersRef.current.get(id)
    if (t != null) { clearTimeout(t); timersRef.current.delete(id) }
  }, [])

  const release = useCallback((id: string) => {
    clearTimer(id)
    setCancellingIds(prev => {
      if (!prev.has(id)) return prev
      const next = new Set(prev)
      next.delete(id)
      return next
    })
  }, [clearTimer])

  const requestCancel = useCallback((id: string) => {
    sendFrameRef.current({ type: 'cancel_task', id })
    setCancellingIds(prev => prev.has(id) ? prev : new Set(prev).add(id))
    clearTimer(id)
    timersRef.current.set(id, setTimeout(() => release(id), CANCEL_ACK_TIMEOUT_MS))
  }, [sendFrameRef, clearTimer, release])

  const handleCancelAck = useCallback((id: string, fired: boolean) => {
    clearTimer(id)
    if (!fired) release(id)
  }, [clearTimer, release])

  // Reconcile against the authoritative registry: drop any latched id whose
  // task is gone or no longer running — the `tasks` frame has superseded it.
  const reconcile = useCallback((tasks: TaskRecord[]) => {
    setCancellingIds(prev => {
      if (prev.size === 0) return prev
      let next: Set<string> | null = null
      for (const id of prev) {
        const t = tasks.find(x => x.task_id === id)
        if (t == null || t.status !== 'running') {
          if (next == null) next = new Set(prev)
          next.delete(id)
          clearTimer(id)
        }
      }
      return next ?? prev
    })
  }, [clearTimer])

  useEffect(() => {
    const timers = timersRef.current
    return () => { timers.forEach(clearTimeout); timers.clear() }
  }, [])

  return { cancellingIds, requestCancel, handleCancelAck, reconcile }
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
      <Touchable style={s.scannerCancel} onPress={onCancel}>
        <Text style={s.scannerCancelText}>Cancel</Text>
      </Touchable>
    </View>
  )
  return (
    <View style={s.scannerFull}>
      <Camera device={device} isActive codeScanner={codeScanner} style={StyleSheet.absoluteFill} />
      <View style={s.scannerOverlay}>
        <View style={s.scannerTopBar}>
          <Image source={require('./assets/icon.png')} style={s.scannerIcon} />
          <Text style={s.scannerTitle}>Scan Session QR</Text>
          <Text style={s.scannerSubtitle}>Point at the code shown by your Okto server</Text>
        </View>
        <View style={[s.scannerReticle, { width: reticleSize, height: reticleSize }]}>
          <View style={[s.scannerCorner, s.cornerTL]} />
          <View style={[s.scannerCorner, s.cornerTR]} />
          <View style={[s.scannerCorner, s.cornerBL]} />
          <View style={[s.scannerCorner, s.cornerBR]} />
        </View>
        <Touchable style={s.scannerCancel} onPress={onCancel}>
          <Text style={s.scannerCancelText}>Cancel</Text>
        </Touchable>
      </View>
    </View>
  )
}

// ── ChatPane ──────────────────────────────────────────────────────────────────

const ChatPane = memo(function ChatPane({
  baseUrl, cacheKey, onStatusChange, clearRef, initialDraft, onDraftChange, reconnectingRef, reloadRef, closeWsRef,
  sendFrameRef, onContainersUpdate, onTasksUpdate, onCancelAck, onTurnEnd,
}: {
  baseUrl:             string
  /// Stable identity for the persisted MMKV history cache. Must NOT include
  /// the local tunnel port (which is ephemeral): pass something like
  /// `master:<lair-pk>` or `agent:<lair-pk>:<agent-name>` so the cache key
  /// survives Noise reconnects.
  cacheKey:            string
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
  /// Push hook for `cancel_task_ack` — the server's receipt confirmation for a
  /// `cancel_task` frame. Lets the STOP-button guard tell a received cancel
  /// apart from a frame dropped on a WS hiccup. See useCancelGuard.
  onCancelAck?:        (id: string, fired: boolean) => void
  /// Fired at every turn boundary (done / interrupted / error). The agent
  /// chat uses it to refresh its worktree list: a turn may have spawned or
  /// torn down a worktree server-side (e.g. the agent ran a tool that removed
  /// one), and lair only pushes `agents` on registry changes — never for
  /// worktree-only changes — so the sidebar would otherwise lag.
  onTurnEnd?:          () => void
}) {
  const insets                     = useSafeAreaInsets()
  const { height: keyboardHeight } = useReanimatedKeyboardAnimation()
  const spacerStyle                = useAnimatedStyle(() => ({
    height: Math.max(insets.bottom, -keyboardHeight.value),
  }))

  // Synchronous hydrate from MMKV. A cache hit lets the chat render its
  // last-known state immediately on mount — no blank list, no full-fade
  // stagger — while loadHistory() reconciles against the server in the
  // background using the existing LCP merge. Status stays 'connecting'
  // until the /stream WS opens: the messages are visible but the user
  // shouldn't be misled into thinking they can send yet.
  const [messages,       setMessages]       = useState<Message[]>(() => {
    const cached = loadCachedHistory(cacheKey)
    return cached ? withPrevRoles(cached as Message[]) : []
  })
  const [status,         setStatus]         = useState<ConnStatus>('connecting')
  // Wire-protocol version advertised by the connected lair (from the `ready`
  // frame); null until the first `ready`. Compared against this build's
  // WIRE_PROTOCOL to surface an "update" banner on a mismatch.
  const [lairProtocol,   setLairProtocol]   = useState<number | null>(null)
  // Ref mirror of `status` so handleStreamEvent (a useCallback that doesn't
  // depend on `status`) can read the current value without closure staleness.
  // Kept in lockstep by updateStatus below.
  const statusRef = useRef<ConnStatus>('connecting')
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
  // Shadow turn state used during a `ready { resumed: true }` → `replay_end`
  // window. While non-null, buffered-event handlers route their setMessages
  // updates into `msgs` here instead of touching the visible state — then the
  // `replay_end` frame swaps the shadow in atomically. Avoids the visible
  // truncate-then-rebuild flash on mid-turn reconnect.
  const replayingRef      = useRef<{ msgs: Message[] } | null>(null)
  // Per-WS counters for the mobile→server keepalive. The WS effect emits
  // `ping { id: ++clientPingNextRef }` every KEEPALIVE_INTERVAL_MS and the
  // server replies with `pong { id }` (handled in handleStreamEvent above,
  // bumps clientPongAckedRef). If clientPingNextRef - clientPongAckedRef
  // reaches KEEPALIVE_MAX_MISSED, we force-close the WS so the existing
  // backoff reconnect kicks in. Symmetrical to the server-side check.
  const clientPingNextRef    = useRef<number>(0)
  const clientPongAckedRef   = useRef<number>(0)
  const listRef           = useRef<FlatList<RenderRow>>(null)
  const isAtBottomRef     = useRef(true)
  const contentHeightRef  = useRef(0)
  const listHeightRef     = useRef(0)
  const historyAbortRef   = useRef<AbortController | null>(null)
  // Staggered history-replay queue. When /history returns more than one new
  // message to append (relative to what's already on screen), we drop the
  // first in immediately and schedule the rest one per `HISTORY_STAGGER_MS`
  // tick so each MessageBubble's fade-in cascades instead of a wall of
  // bubbles fading in together. Cleared on unmount and on each new load.
  const historyStaggerRef = useRef<{
    queue: Message[]
    timer: ReturnType<typeof setTimeout> | null
  }>({ queue: [], timer: null })

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
    statusRef.current = s
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

    // Route a message-list update either to the visible state, or — while a
    // replay window is open — into the shadow that `replay_end` will swap in.
    const applyMsgs = (updater: (prev: Message[]) => Message[]) => {
      if (replayingRef.current) {
        replayingRef.current.msgs = updater(replayingRef.current.msgs)
      } else {
        setMessages(updater)
      }
    }

    // Defensive: text/tool_use events arrive only when the model is actively
    // producing output. For a user-initiated turn, `sendMessage` already
    // flipped status to 'streaming' before the user_message frame went out.
    // But the server also auto-injects follow-up turns we never asked for —
    // e.g. after a `bg_complete`, the model continues with text/tool_use
    // events with no preceding client trigger. Without this defensive flip,
    // status stays 'ready' and the Send button doesn't get replaced by the
    // stop/spinner for the duration of that turn. Skipped during replay —
    // the `ready { resumed: true }` handler manages status for that path.
    if (!replayingRef.current && statusRef.current !== 'streaming' &&
        (event.type === 'text' || event.type === 'tool_use')) {
      updateStatus('streaming')
    }

    switch (event.type) {
      case 'ready': {
        // Record the wire-protocol version the lair we're connected to speaks
        // (absent on lairs older than protocol 1 → treated as 0). The banner
        // above the transcript compares it to WIRE_PROTOCOL (what this build
        // speaks) and tells the user which side to update on a mismatch.
        setLairProtocol(typeof event.wire_protocol === 'number' ? event.wire_protocol : 0)
        // Server greets us; status becomes 'streaming' if we joined an in-flight
        // turn (events for it will arrive next), else 'ready' for input.
        updateStatus(event.resumed ? 'streaming' : 'ready')
        if (event.resumed) {
          // The server's buffer holds the *entire* in-flight turn from its
          // first event and is about to be replayed. Stash a truncated copy
          // of the visible state — back to the last turn anchor — as the
          // shadow that the buffered events will rebuild into. The visible
          // list stays untouched until `replay_end` swaps the shadow in, so
          // the user never sees a truncate-then-rebuild flash. Clear any
          // stale `running` flags while we're at it: a previous WS drop may
          // have missed tool_results and the dots would otherwise blink on.
          //
          // First drain any in-flight history-stagger queue. On foreground
          // return loadHistory reconciles a multi-row suffix (completed turns
          // + their bg anchors since the last user message) by appending it
          // one row per tick. If `ready { resumed: true }` lands mid-stagger,
          // those rows are still queued — not yet in `prev` — so the anchor
          // scan below would miss the latest bg row and snapshot too short a
          // shadow. The replay only rebuilds the *current* turn, so anything
          // the stagger appends after this snapshot would then be wiped by
          // `replay_end`'s swap. Folding the queue in now keeps those
          // completed turns in the shadow (and lands the anchor on the real
          // last bg row), so the swap is lossless.
          const staggered = historyStaggerRef.current.queue
          historyStaggerRef.current.queue = []
          if (historyStaggerRef.current.timer) {
            clearTimeout(historyStaggerRef.current.timer)
            historyStaggerRef.current.timer = null
          }
          setMessages(prev => {
            const merged = staggered.length
              ? [...prev, ...staggered.filter(s => !prev.some(m => m.id === s.id))]
              : prev
            const hadRunning = merged.some(m => m.running)
            const cleaned = hadRunning ? merged.map(m => m.running ? { ...m, running: false } : m) : merged
            let anchor = cleaned
            for (let i = cleaned.length - 1; i >= 0; i--) {
              const role = cleaned[i].role
              if (role === 'user' || role === 'bg_complete' || role === 'bg_progress') {
                anchor = cleaned.slice(0, i + 1)
                break
              }
            }
            replayingRef.current = { msgs: anchor }
            return cleaned
          })
        } else {
          // Not resumed — drop any stale shadow from a previous mid-replay drop.
          replayingRef.current = null
        }
        // Reset per-turn streaming refs unconditionally: for resumed=false this
        // is the first turn after connect; for resumed=true the replay restarts
        // the turn from its first event (events accumulate into the shadow).
        streamingIdRef.current = uid()
        hasAssistantMsgRef.current = false
        break
      }
      case 'replay_end': {
        // Server has finished replaying the in-flight turn's buffered events
        // into our shadow. Swap it in as a single atomic update so the user
        // sees no intermediate state.
        if (replayingRef.current) {
          const next = replayingRef.current.msgs
          replayingRef.current = null
          setMessages(_ => next)
        }
        break
      }
      case 'text': {
        const chunk = event.text
        if (!hasAssistantMsgRef.current) {
          hasAssistantMsgRef.current = true
          const id = streamingIdRef.current
          applyMsgs(prev => appendMsg(prev, { id, role: 'assistant' as const, text: chunk }))
        } else {
          const id = streamingIdRef.current
          applyMsgs(prev => prev.map(m => m.id === id ? { ...m, text: m.text + chunk } : m))
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
        log(`[chat] tool_use tool=${event.tool} id=${event.tool_use_id}`)
        // Use the wire tool_use_id directly as the Message id so subsequent
        // tool_output / tool_result events route to the right bubble even when
        // the model emits multiple tool_use blocks in one turn.
        //
        // The model can emit several tool_use blocks at once and they all
        // stream to mobile before the server executes any of them. Mark this
        // tool `running` only if no earlier tool is still executing; otherwise
        // it's queued and gets promoted when the active one's tool_result
        // arrives. Mirrors the server-side sequential execution.
        applyMsgs(prev => {
          const anyRunning = prev.some(m => m.running)
          return appendMsg(prev, { id: event.tool_use_id, role: 'tool' as const, text: toolText, running: !anyRunning })
        })
        break
      }
      case 'tool_output': {
        const toolId = event.tool_use_id
        applyMsgs(prev => prev.map(m =>
          m.id === toolId ? { ...m, output: (m.output ?? '') + event.line + '\n' } : m
        ))
        break
      }
      case 'tool_result': {
        const toolId = event.tool_use_id
        const out = typeof event.output === 'string' ? event.output : JSON.stringify(event.output)
        applyMsgs(prev => {
          const completedIdx = prev.findIndex(m => m.id === toolId)
          // Promote the next queued tool (earliest after the completed one,
          // not running, no output yet) to active execution. Tools run in
          // emission order so the next queued slot is always after the
          // current one in the array.
          let nextQueuedIdx = -1
          for (let i = completedIdx + 1; i < prev.length; i++) {
            const m = prev[i]
            if (m.role === 'tool' && !m.running && m.output === undefined) {
              nextQueuedIdx = i
              break
            }
          }
          return prev.map((m, i) => {
            if (i === completedIdx)  return { ...m, output: out, running: false }
            if (i === nextQueuedIdx) return { ...m, running: true }
            return m
          })
        })
        break
      }
      case 'done': {
        log(`[chat] stream done cost_usd=${event.cost_usd}`)
        updateStatus('ready')
        const cost = event.cost_usd
        applyMsgs(prev => {
          // Defensive: every tool_use should have been matched by a
          // tool_result before the turn ends, but a dropped frame would
          // otherwise leave the dot blinking forever.
          const base = prev.some(m => m.running)
            ? prev.map(m => m.running ? { ...m, running: false } : m)
            : prev
          for (let i = base.length - 1; i >= 0; i--) {
            if (base[i].role === 'assistant') {
              const next = base.slice()
              next[i] = { ...next[i], cost }
              return next
            }
          }
          return base
        })
        // Turn boundary — anything streamed after this (auto-turn from a
        // bg_complete, or the next user turn) must start a fresh assistant
        // bubble, not append to the one we just sealed.
        hasAssistantMsgRef.current = false
        streamingIdRef.current = uid()
        // Reconcile against the server's authoritative history at every turn
        // boundary. If this turn was driven by another client (we're a passive
        // listener), our local list may have appended the streamed assistant
        // rows onto a stale baseline — e.g. the conversation was cleared
        // elsewhere, or we never saw the other client's user message. The LCP
        // merge snaps us back to the server's history. For the client that
        // drove the turn this is a no-op (local already matches /history).
        loadHistoryRef.current()
        onTurnEnd?.()
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
        updateStatus('ready')
        const cost = event.cost_usd
        applyMsgs(prev => {
          // Any tool still marked as running won't get a tool_result now.
          prev = prev.map(m => m.running ? { ...m, running: false } : m)
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
        onTurnEnd?.()
        break
      }
      case 'error':
        logE(`[chat] stream error: ${event.message}`)
        applyMsgs(prev => appendMsg(prev.map(m => m.running ? { ...m, running: false } : m), { id: uid(), role: 'error' as const, text: event.message }))
        updateStatus('ready')
        hasAssistantMsgRef.current = false
        streamingIdRef.current = uid()
        onTurnEnd?.()
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
      case 'cancel_task_ack':
        log(`[chat] cancel_task_ack id=${event.id} fired=${event.fired}`)
        if (onCancelAck) onCancelAck(event.id, event.fired)
        break
      case 'bg_complete': {
        // Live insertion of the bg_complete chip between assistant turns. The
        // id is stable per task so a subsequent /history reload (which also
        // contains this row) is a no-op rather than a duplicate.
        const id = `bg_${event.task_id}`
        applyMsgs(prev => prev.some(m => m.id === id)
          ? prev
          : appendMsg(prev, { id, role: 'bg_complete' as const, text: event.text }))
        break
      }
      case 'bg_progress':
        // A monitored task produced new output mid-run. Each event is distinct
        // output, so it gets its own chip. The event text matches the persisted
        // bg_progress row, so a later /history reload reconciles cleanly.
        applyMsgs(prev => appendMsg(prev, { id: uid(), role: 'bg_progress' as const, text: event.text }))
        break
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
  }, [updateStatus, sendFrame, onContainersUpdate, onTasksUpdate, onCancelAck, onTurnEnd])

  // Keep a stable ref to loadHistory so reattachStream can call it without
  // being listed as a dependency (avoids circular dep: loadHistory → reattachStream → loadHistory).
  const loadHistoryRef = useRef<() => void>(() => {})

  // Drains the staggered history queue one message per tick. Each append
  // remounts a single bubble whose own 180ms fade-in (MessageBubble) then
  // cascades into the next tick, producing a smooth load-in instead of an
  // all-at-once flicker. Idempotent against live-stream events that may
  // race in: dedupe by id before appending.
  const tickHistoryStagger = useCallback(() => {
    const stagger = historyStaggerRef.current
    const next = stagger.queue.shift()
    if (next === undefined) {
      stagger.timer = null
      return
    }
    setMessages(prev => prev.some(m => m.id === next.id) ? prev : [...prev, next])
    stagger.timer = stagger.queue.length > 0
      ? setTimeout(tickHistoryStagger, HISTORY_STAGGER_MS)
      : null
  }, [])

  const loadHistory = useCallback((attempt = 0) => {
    historyAbortRef.current?.abort()
    const controller = new AbortController()
    historyAbortRef.current = controller
    log(`[chat] loadHistory GET ${baseUrl}/history${attempt > 0 ? ` (attempt ${attempt + 1})` : ''}`)
    fetch(`${baseUrl}/history`, { signal: controller.signal })
      .then(r => { log(`[chat] loadHistory HTTP ${r.status}`); return r.json() })
      .then((data: { messages: Array<{ role: string; text: string; cost_usd?: number; output?: string; display?: string }> }) => {
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
          // Tool rows carry a friendly `display` ("Editing file (…)") that
          // mirrors what the live stream showed; prefer it so a finished tool
          // never reverts to the raw `name(arg)` text.
          text: m.display ?? m.text,
          ...(m.cost_usd != null ? { cost: m.cost_usd } : {}),
          ...(m.output    != null ? { output: m.output } : {}),
        })))
        // Reconcile the live conversation against the server's canonical
        // history with a longest-common-prefix merge: matched rows are kept
        // verbatim so they retain their ids and FlatList reuses the mounted
        // bubbles. A naive full replace re-keys every row, remounting the
        // whole list and re-running each bubble's fade-in — the flicker.
        //
        // For the divergent suffix (new rows from history), we drop the
        // first in immediately and queue the rest for the staggered
        // ticker so each fades in one-after-the-next rather than as one
        // simultaneous wall.
        if (historyStaggerRef.current.timer) {
          clearTimeout(historyStaggerRef.current.timer)
          historyStaggerRef.current.timer = null
        }
        historyStaggerRef.current.queue = []
        setMessages(cur => {
          // Tool rows are matched leniently: the client renders a tool as
          // `label (arg)` while /history projects it as `name(arg)`, so a
          // strict text compare would diverge on the first tool row and
          // force a full replace. They're the same event — match by role
          // and keep the client's already-rendered row.
          const eq = (a: Message, b: Message) => {
            if (a.role !== b.role) return false
            if (a.role === 'tool') return true
            return a.text === b.text && a.cost === b.cost && a.output === b.output
          }
          let common = 0
          while (common < cur.length && common < msgs.length && eq(cur[common], msgs[common])) common++
          if (common === msgs.length) {
            // Identical to what we already have — nothing to apply.
            if (common === cur.length) return cur
            // Server history is a strict prefix of ours. Either a turn is
            // streaming/replaying and its rows aren't persisted to /history
            // yet (keep them — we're live-ahead), or the conversation was
            // cleared/truncated on the server, e.g. from another client
            // (adopt the server's shorter history). /history is authoritative
            // when idle, so truncate the stale tail.
            if (statusRef.current === 'streaming' || replayingRef.current) return cur
            return cur.slice(0, common)
          }
          const suffix = msgs.slice(common)
          // Single new row — append directly; no need to engage the ticker.
          if (suffix.length === 1) {
            return [...cur.slice(0, common), suffix[0]]
          }
          // Multiple new rows — first goes in synchronously so the user
          // sees motion immediately; the rest land one per stagger tick.
          historyStaggerRef.current.queue = suffix.slice(1)
          historyStaggerRef.current.timer = setTimeout(tickHistoryStagger, HISTORY_STAGGER_MS)
          return [...cur.slice(0, common), suffix[0]]
        })
        // Status is driven entirely by /stream events now (`ready` on connect,
        // `done`/`interrupted`/`error` at turn end), so loadHistory no longer
        // needs to drive it from `is_streaming`.
        setTimeout(() => {
          // Only re-pin to the bottom if the user was already there.
          // Otherwise they're reading earlier content and a history reconcile
          // (e.g. end-of-turn, foreground return) would yank them away.
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
    return () => {
      historyAbortRef.current?.abort()
      if (historyStaggerRef.current.timer) {
        clearTimeout(historyStaggerRef.current.timer)
        historyStaggerRef.current.timer = null
      }
      historyStaggerRef.current.queue = []
    }
  }, [baseUrl])

  // Persist messages to MMKV whenever the chat is in a settled state. We
  // deliberately skip writes while status === 'streaming' so a mid-flight
  // assistant bubble (partial text) never lands in the cache — a partial
  // would later collide with /history's complete version under the LCP
  // reconcile and produce a duplicated assistant row on rehydrate. The
  // 250 ms debounce coalesces bursts (e.g. a series of bg_progress chips
  // or the rapid setState at turn end) into a single write.
  useEffect(() => {
    if (status === 'streaming') return
    if (messages.length === 0) {
      // Empty array (fresh chat or post-clear) — drop the key rather than
      // persisting [], so the next mount short-circuits past the cache.
      clearCachedHistory(cacheKey)
      return
    }
    const timer = setTimeout(() => {
      saveCachedHistory(cacheKey, messages)
    }, 250)
    return () => clearTimeout(timer)
  }, [messages, status, cacheKey])

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

  // Coalesce runs of ≥2 consecutive tool rows into a single ToolGroup so a long
  // agentic loop doesn't flood the transcript; a lone tool renders inline. The
  // group's stable key is its first tool's id so the FlatList row survives as
  // later tools stream in and extend the run.
  const renderRows = useMemo<RenderRow[]>(() => {
    const rows: RenderRow[] = []
    for (let i = 0; i < messages.length; i++) {
      if (messages[i].role === 'tool') {
        const run: Message[] = []
        while (i < messages.length && messages[i].role === 'tool') { run.push(messages[i]); i++ }
        i--
        if (run.length === 1) {
          rows.push({ kind: 'msg', key: run[0].id, message: run[0] })
        } else {
          rows.push({ kind: 'group', key: `grp:${run[0].id}`, tools: run })
        }
      } else {
        rows.push({ kind: 'msg', key: messages[i].id, message: messages[i] })
      }
    }
    return rows
  }, [messages])

  const renderMessageItem = useCallback(({ item }: { item: RenderRow }) => (
    item.kind === 'group'
      ? <ToolGroup tools={item.tools} />
      : <MessageBubble message={item.message} prevRole={item.message.prevRole} />
  ), [])

  // Advisory when the connected lair speaks a different wire protocol than this
  // build. lair stays backward-compatible across versions, so this is a soft
  // nudge (not a hard block): point the user at whichever side is behind.
  const wireNotice = lairProtocol == null || lairProtocol === WIRE_PROTOCOL
    ? null
    : lairProtocol < WIRE_PROTOCOL
      ? 'Your okto host is on an older version. Update it (okto lair update) for full compatibility.'
      : 'This app is older than your okto host. Update the app for full compatibility.'

  return (
    <View flex={1} backgroundColor="$background50">
      {wireNotice && (
        <View backgroundColor="$primary50" paddingHorizontal={16} paddingVertical={8} borderBottomWidth={1} borderColor="$outline200">
          <Text fontFamily="$body" fontSize={12} lineHeight={17} color="$primary700">⚠ {wireNotice}</Text>
        </View>
      )}
      <View flex={1}>
        <FlatList
          ref={listRef}
          data={renderRows}
          keyExtractor={r => r.key}
          renderItem={renderMessageItem}
          contentContainerStyle={[
            { paddingVertical: 18 },
            { paddingBottom: inputAreaH + 8 },
            status === 'error' && { paddingTop: 34 },
          ]}
          style={{ flex: 1 }}
          ListEmptyComponent={
            <View marginTop={88} alignItems="center" gap={8}>
              <AppIcon />
              <View marginTop={4} height={2} width={32} borderRadius={999} backgroundColor="$primary600" opacity={0.6} />
              <Text marginTop={8} fontSize={11} fontWeight="600" textTransform="uppercase" letterSpacing={1.8} color="$typography500" style={{ fontFamily: MONO }}>Awaiting Instructions</Text>
            </View>
          }
          onContentSizeChange={(_, h) => {
            contentHeightRef.current = h
            // Follow new content — new messages and streaming text — only
            // while the user is already at the bottom. If they've scrolled
            // up to read, leave the viewport put; the scroll-to-bottom
            // button handles re-pinning.
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
        />


        {completions.length > 0 && (
          <ScrollView
            style={{ bottom: inputAreaH, position: 'absolute', left: 8, right: 8, zIndex: 10, maxHeight: 180, overflow: 'hidden', borderRadius: 14, borderWidth: 1, borderColor: 'rgb(221,220,219)', backgroundColor: '#fff' }}
            keyboardShouldPersistTaps="handled"
            showsVerticalScrollIndicator={false}
          >
            {completions.map(c => (
              <Touchable key={c} borderBottomWidth={1} borderColor="$outline200" paddingHorizontal={16} paddingVertical={11} onPress={() => applyCompletion(c)}>
                <Text fontSize={13.5} color="$typography900" style={{ fontFamily: MONO }}>{c}</Text>
              </Touchable>
            ))}
          </ScrollView>
        )}
        <View position="absolute" left={0} right={0} bottom={0} borderTopWidth={1} borderColor="$outline200" backgroundColor="$background50" paddingHorizontal={12} paddingBottom={12} paddingTop={10} onLayout={e => setInputAreaH(e.nativeEvent.layout.height)}>
          <View flexDirection="row" alignItems="flex-end" gap={10}>
            <TextInput
              testID="chat-input"
              // Lock height to one line when empty so a `setInput('')` on send
              // collapses the box immediately — without this, iOS keeps the
              // previous multiline intrinsic size for ~a second. With a value
              // present we omit `height` so RN's intrinsic sizing handles
              // multiline auto-grow (clamped by min/max height classes).
              style={[{ maxHeight: 140, minHeight: 56, flex: 1, borderRadius: 22, borderWidth: 1, borderColor: 'rgb(221,220,219)', backgroundColor: '#fff', paddingHorizontal: 18, paddingVertical: 15, fontFamily: ARIMO, fontSize: 16, lineHeight: 22, color: 'rgb(15,23,42)' }, !input ? { height: 56 } : null]}
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
              <View height={56} width={56} alignItems="center" justifyContent="center">
                <OrbitingArc size={56} thickness={3} />
                <Touchable
                  testID="composer-stop"
                  height={50} width={50} alignItems="center" justifyContent="center" borderRadius={25} backgroundColor="$error500" opacity={stopSent ? 0.4 : undefined}
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
                  <Text fontSize={18} fontWeight="700" color="$typography0">■</Text>
                </Touchable>
              </View>
            ) : (
              <Touchable
                testID="composer-send"
                height={56} width={56} alignItems="center" justifyContent="center" borderRadius={22} borderWidth={!input.trim() ? 1 : undefined} borderColor={!input.trim() ? '$outline200' : undefined} backgroundColor={!input.trim() ? '$background100' : '$primary600'}
                onPress={() => sendMessageRef.current()}
                disabled={!input.trim()}
                activeOpacity={0.75}
              >
                <Send size={22} color={!input.trim() ? '#94A3B8' : '#FFFFFF'} />
              </Touchable>
            )}
          </View>
        </View>

        {showScrollBtn && (
          <View position="absolute" left={0} right={0} alignItems="center" style={{ bottom: inputAreaH }} pointerEvents="box-none">
            <Touchable
              marginBottom={12} height={36} width={36} alignItems="center" justifyContent="center" borderRadius={999} borderWidth={1} borderColor="$outline200" backgroundColor="$background0"
              onPress={() => {
                isAtBottomRef.current = true
                setShowScrollBtn(false)
                const offset = Math.max(0, contentHeightRef.current - listHeightRef.current)
                listRef.current?.scrollToOffset({ offset, animated: true })
              }}
              activeOpacity={0.75}
            >
              <Text fontSize={16} fontWeight="700" color="$typography900" style={{ fontFamily: ARIMO }}>↓</Text>
            </Touchable>
          </View>
        )}

        {status === 'error' && (
          <View position="absolute" left={0} right={0} top={0} zIndex={10} flexDirection="row" alignItems="center" justifyContent="center" borderBottomWidth={1} borderColor="$error600" backgroundColor="$background100" paddingVertical={7} pointerEvents="none">
            <Text fontSize={11} fontWeight="700" textTransform="uppercase" letterSpacing={1.4} color="$error600" style={{ fontFamily: MONO }}>Connection error</Text>
          </View>
        )}
      </View>
      <Reanimated.View style={[{ backgroundColor: C.surface }, spacerStyle]} />
    </View>
  )
})


// ── ConnectionErrorModal ────────────────────────────────────────────────────────

function ConnectionErrorModal({ visible, onDismiss }: { visible: boolean; onDismiss: () => void }) {
  const slide = useRef(new Animated.Value(0)).current
  const [mounted, setMounted] = useState(false)

  useEffect(() => {
    if (visible) setMounted(true)
    Animated.timing(slide, {
      toValue:        visible ? 1 : 0,
      duration:       200,
      useNativeDriver: true,
    }).start(({ finished }) => {
      if (finished && !visible) setMounted(false)
    })
  }, [visible])

  if (!mounted) return null

  return (
    <View style={StyleSheet.absoluteFill} pointerEvents="box-none">
      <Animated.View
        style={[StyleSheet.absoluteFill, { zIndex: 400, opacity: slide, backgroundColor: 'rgba(2,6,23,0.5)' }]}
        pointerEvents={visible ? 'auto' : 'none'}
      >
        <Touchable style={StyleSheet.absoluteFill} activeOpacity={1} onPress={onDismiss} />
      </Animated.View>
      <Animated.View
        style={{
          position: 'absolute', alignItems: 'center', borderRadius: 18, backgroundColor: '#fff', padding: 28,
          top: '38%', left: '12%', right: '12%', zIndex: 401,
          transform: [{ scale: slide.interpolate({ inputRange: [0, 1], outputRange: [0.9, 1] }) }],
          opacity: slide,
        }}
        pointerEvents={visible ? 'auto' : 'none'}
      >
        <View marginBottom={14} height={52} width={52} alignItems="center" justifyContent="center" borderRadius={999} backgroundColor="$error50">
          <Text fontFamily="$heading" fontSize={28} fontWeight="700" color="$error600">!</Text>
        </View>
        <Text marginBottom={6} fontFamily="$heading" fontSize={18} fontWeight="700" color="$typography900">Connection Lost</Text>
        <Text marginBottom={20} textAlign="center" fontFamily="$body" fontSize={14} lineHeight={20} color="$typography500">Reconnecting automatically…{'\n'}Tap to dismiss.</Text>
        <Button onPress={onDismiss} borderRadius={999} paddingHorizontal={28} backgroundColor="$primary600">
          <ButtonText color="$typography0">Dismiss</ButtonText>
        </Button>
      </Animated.View>
    </View>
  )
}

// ── ChildChatScreen ────────────────────────────────────────────────────────────

function ChildChatScreen({ child, worktree, tunnelPort, tunnelError, cacheKey, onOpenSidebar, initialDraft, onDraftChange, reconnectingRef, reloadRef, closeWsRef, onTurnEnd }: {
  child:             ContainerInfo
  /// When set, this pane is a git worktree of `child` — chat is scoped to the
  /// worktree's proxy path and the header shows its branch.
  worktree?:         WorktreeMeta | null
  tunnelPort:        number | null
  tunnelError:       string | null
  /// Stable identity for the MMKV history cache. See ChatPane.cacheKey.
  cacheKey:          string
  onOpenSidebar:     () => void
  initialDraft?:     string
  onDraftChange?:    (draft: string) => void
  reconnectingRef?:  React.MutableRefObject<boolean>
  reloadRef?:        React.MutableRefObject<() => void>
  closeWsRef?:       React.MutableRefObject<() => void>
  /// Called with this agent's name at every turn boundary, so the parent can
  /// refresh the agent's worktree list (a turn may have spawned/removed one).
  onTurnEnd?:        (agentName: string) => void
}) {
  const [chatStatus, setChatStatus] = useState<ConnStatus>('connecting')
  const [dismissedConnError, setDismissedConnError] = useState(false)
  // Reset dismissed flag when status leaves 'error' so next error re-shows the modal.
  useEffect(() => { if (chatStatus !== 'error') setDismissedConnError(false) }, [chatStatus])
  const [tasks,      setTasks]      = useState<TaskRecord[]>([])
  const [showTasksModal, setShowTasksModal] = useState(false)
  const clearRef     = useRef<() => void>(() => {})
  const sendFrameRef = useRef<(frame: ClientFrame) => boolean>(() => false)
  const { cancellingIds, requestCancel, handleCancelAck, reconcile } = useCancelGuard(sendFrameRef)
  // Stable identity so the memoized ChatPane isn't re-rendered on every
  // task-progress tick.
  const handleTasksUpdate = useCallback((t: TaskRecord[]) => {
    setTasks(t)
    reconcile(t)
  }, [reconcile])
  // Stable so ChatPane's memo isn't broken every render. Reports this agent's
  // name up at each turn boundary for a worktree-list refresh.
  const handleTurnEnd = useCallback(() => { onTurnEnd?.(child.name) }, [onTurnEnd, child.name])

  return (
    // No SafeAreaView here: this screen is rendered as an overlay inside
    // AppInner's SafeAreaView, so applying the top inset again would push
    // the header down by double the status-bar height.
    <View flex={1} backgroundColor="$background50">
      <View flex={1}>
        <View flexDirection="row" alignItems="center" justifyContent="space-between" borderBottomWidth={1} borderColor="$outline200" backgroundColor="$background50" paddingHorizontal={16} paddingVertical={12}>
          <View flex={1} flexDirection="row" alignItems="center" gap={10}>
            <Touchable
              testID="open-sidebar-child"
              marginRight={4} paddingHorizontal={6} paddingVertical={8}
              onPress={onOpenSidebar}
              hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
            >
              <View height={12} width={18} justifyContent="space-between">
                <View height={2} borderRadius={999} backgroundColor="$typography900" />
                <View height={2} borderRadius={999} backgroundColor="$typography900" />
                <View height={2} borderRadius={999} backgroundColor="$typography900" />
              </View>
            </Touchable>
            <Text fontFamily="$body" fontSize={15} fontWeight="700" letterSpacing={0.2} color="$typography900">
              {containerDisplayName(child.name)}{worktree ? ` / ${worktree.branch}` : ''}
            </Text>
          </View>
          <View flexDirection="row" alignItems="center" gap={8}>
            <TasksHeaderButton tasks={tasks} onPress={() => { Keyboard.dismiss(); setShowTasksModal(true) }} />
            <Touchable
              borderRadius={999} borderWidth={1} borderColor="$outline200" backgroundColor="$background0" paddingHorizontal={12} paddingVertical={5}
              onPress={() => clearRef.current()}
              hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
              disabled={chatStatus !== 'ready'}
            >
              <Text fontFamily="$body" fontSize={11} fontWeight="600" letterSpacing={0.4} color="$typography600" opacity={chatStatus !== 'ready' ? 0.3 : undefined}>clear</Text>
            </Touchable>
          </View>
        </View>

        {tunnelPort ? (
          <ChatPane
            baseUrl={`http://127.0.0.1:${tunnelPort}/agents/${child.name}${worktree ? `/worktrees/${worktree.id}` : ''}`}
            cacheKey={cacheKey}
            onStatusChange={setChatStatus}
            clearRef={clearRef}
            initialDraft={initialDraft}
            onDraftChange={onDraftChange}
            reconnectingRef={reconnectingRef}
            reloadRef={reloadRef}
            closeWsRef={closeWsRef}
            sendFrameRef={sendFrameRef}
            onTasksUpdate={handleTasksUpdate}
            onCancelAck={handleCancelAck}
            onTurnEnd={handleTurnEnd}
          />
        ) : tunnelError ? (
          <View flex={1} alignItems="center" justifyContent="center" gap={14} paddingHorizontal={40}>
            <Text overflow="hidden" borderRadius={12} backgroundColor="$error50" paddingHorizontal={14} paddingVertical={10} textAlign="center" fontFamily="$body" fontSize={13} lineHeight={19} color="$error600">{tunnelError}</Text>
          </View>
        ) : null}

        <TasksModal
          visible={showTasksModal}
          tasks={tasks}
          cancellingIds={cancellingIds}
          onClose={() => setShowTasksModal(false)}
          onCancel={(id) => {
            log(`[child] cancel_task id=${id}`)
            requestCancel(id)
          }}
        />

        <ConnectionErrorModal
          visible={chatStatus === 'error' && !dismissedConnError}
          onDismiss={() => setDismissedConnError(true)}
        />
      </View>
    </View>
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
        <RNView style={{ flex: 1, backgroundColor: '#EB4F0B', alignItems: 'center', justifyContent: 'center', padding: 32 }}>
          <RNText style={{ color: '#fff', fontSize: 18, fontWeight: '700', marginBottom: 12 }}>Something went wrong</RNText>
          <RNText style={{ color: 'rgba(255,255,255,0.85)', fontSize: 14, textAlign: 'center', lineHeight: 20 }}>{this.state.error}</RNText>
        </RNView>
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
  const [manualConn,  setManualConn]  = useState('')
  const [manualError, setManualError] = useState<string | null>(null)
  // Hooks must run unconditionally — keep this above the early returns for the
  // scan / sign-in screens, not down by the chat layout that uses it.
  const { width: screenW } = useWindowDimensions()
  // Setup-screen keyboard avoidance: when the manual-connect TextInput is
  // focused, animate paddingBottom up by the keyboard height. The setup
  // content is `justifyContent: 'center'`, so shrinking the available area
  // from the bottom shifts the content up by half the keyboard height —
  // enough to clear the input without pushing the AppIcon off the top.
  const { height: kbHeight } = useReanimatedKeyboardAnimation()
  const setupKeyboardLift = useAnimatedStyle(() => ({
    paddingBottom: Math.max(0, -kbHeight.value),
  }))
  const [chatStatus,  setChatStatus]  = useState<ConnStatus>('connecting')
  const [dismissedConnError, setDismissedConnError] = useState(false)
  // Reset dismissed flag when status leaves 'error' so next error re-shows the modal.
  useEffect(() => { if (chatStatus !== 'error') setDismissedConnError(false) }, [chatStatus])
  const [containers,          setContainers]          = useState<ContainerInfo[]>([])
  const [activeChild,         setActiveChild]         = useState<ContainerInfo | null>(null)
  // Git worktree this pane is scoped to, paired with `activeChild`. `null` when
  // viewing the agent's main workspace chat.
  const [activeWorktree,      setActiveWorktree]      = useState<WorktreeMeta | null>(null)
  // When swapping between two child agents, the previously-shown child is
  // parked here so its pane stays visible underneath the incoming slide-in.
  // Without it the swap would briefly reveal the master pane behind, since
  // resetting `childSlideAnim` to 0 also slides the master back into view.
  const [outgoingChild,       setOutgoingChild]       = useState<ContainerInfo | null>(null)
  const [outgoingWorktree,    setOutgoingWorktree]    = useState<WorktreeMeta | null>(null)
  // Worktrees per agent name, fetched from GET /agents/:name/worktrees and
  // rendered as indented rows under their agent in the sidebar.
  const [worktrees,           setWorktrees]           = useState<Record<string, WorktreeMeta[]>>({})
  // Agent name whose inline "new branch" input is showing in the sidebar, and
  // the in-progress branch text.
  const [creatingWtFor,       setCreatingWtFor]       = useState<string | null>(null)
  const [newBranchDraft,      setNewBranchDraft]      = useState<string>('')
  // Per-chat background-task registry for the master chat. Pushed by lair on
  // /stream open and after every spawn / completion / cancellation.
  const [masterTasks,         setMasterTasks]         = useState<TaskRecord[]>([])
  const [showTasksModal,      setShowTasksModal]      = useState(false)
  const [showSidebar,    setShowSidebar]    = useState(false)
  const sidebarAnim = useRef(new Animated.Value(0)).current
  const childSlideAnim = useRef(new Animated.Value(0)).current
  const [childMounted, setChildMounted] = useState(false)
  const [startingContainerId, setStartingContainerId] = useState<string | null>(null)
  const [startingError,       setStartingError]       = useState<string | null>(null)
  // (Was a `reconnecting` state driving a full-screen "Connecting..."
  // overlay during foreground-return reconnects. Removed in favour of
  // showing the chat continuously — the silent reconnect + smooth history
  // reconcile is good enough that flashing an overlay just looks like a
  // load flicker. `reconnectingRef` below still suppresses the per-WS
  // "connecting" status flash so the chat header stays calm.)
  const startingContainerIdRef = useRef<string | null>(null)
  const openChatRef = useRef((_child: ContainerInfo, _wt?: WorktreeMeta | null) => {})
  const clearChatRef       = useRef<() => void>(() => {})
  const reloadRef          = useRef<() => void>(() => {})
  const closeWsRef         = useRef<() => void>(() => {})
  // Bound to the master ChatPane's persistent /stream WS once it's open.
  // Returns false if no WS is connected (caller should surface or retry).
  const masterSendFrameRef = useRef<(frame: ClientFrame) => boolean>(() => false)
  const {
    cancellingIds: masterCancellingIds,
    requestCancel: masterRequestCancel,
    handleCancelAck: masterHandleCancelAck,
    reconcile: masterReconcile,
  } = useCancelGuard(masterSendFrameRef)
  // Stable identity so the memoized ChatPane isn't re-rendered on every
  // task-progress tick.
  const handleMasterTasksUpdate = useCallback((t: TaskRecord[]) => {
    setMasterTasks(t)
    masterReconcile(t)
  }, [masterReconcile])
  // In-memory draft cache: survives ChatPane unmount/remount without async latency.
  const draftsRef          = useRef<Record<string, string>>({})
  // Held true for the full duration of a foreground-return reconnect so that
  // WS error/close callbacks know not to surface a connection-error to the user.
  const reconnectingRef    = useRef<boolean>(false)
  // Ref mirrors of conn/activeChild so the imperative reconnect() can read
  // current values without being a useCallback dependency.
  const connRef            = useRef<NoiseConnectionInfo | null>(null)
  const activeChildRef     = useRef<ContainerInfo | null>(null)
  const activeWorktreeRef  = useRef<WorktreeMeta | null>(null)
  useEffect(() => { connRef.current = conn },         [conn])
  useEffect(() => { activeChildRef.current = activeChild }, [activeChild])
  useEffect(() => { activeWorktreeRef.current = activeWorktree }, [activeWorktree])

  // The single Noise tunnel always points at lair. Mobile reaches a child
  // agent's chat by opening WS to `/agents/<name>/stream` over the same
  // tunnel — lair proxies the traffic.
  const masterBaseUrl = tunnelPort ? `http://127.0.0.1:${tunnelPort}` : null
  // Stable identity for MMKV history caches. Survives Noise tunnel
  // reconnects (which churn the ephemeral local proxy port) by keying off
  // the lair's Noise public key rather than its loopback URL.
  const lairPk          = conn?.pk ?? ''
  const masterCacheKey  = `master:${lairPk}`

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
        openChatRef.current(started)
      }
    }
  }, [])

  // If the worktree we're actively viewing has vanished from `agentName`'s
  // authoritative list (deleted from another client, or torn down server-side
  // by the agent itself), its chat pane would otherwise linger as a ghost —
  // header stuck on `<agent> / <branch>`, socket dead. Fall back to the parent
  // agent's chat. Mirrors desktop's reconcileWorktrees. Guard on the agent
  // name too: worktree ids are per-agent slugs, so two agents can share one
  // (e.g. both "feature-x").
  const reconcileActiveWorktree = useCallback((agentName: string, live: WorktreeMeta[]) => {
    const wt = activeWorktreeRef.current
    const child = activeChildRef.current
    if (wt && child?.name === agentName && !live.some(w => w.id === wt.id)) {
      openChatRef.current(child)
    }
  }, [])

  // Apply a freshly-fetched worktree list for one agent: update the sidebar
  // nesting and drop a now-ghost worktree chat if one is active.
  const applyWorktrees = useCallback((agentName: string, wts: WorktreeMeta[]) => {
    setWorktrees(prev => ({ ...prev, [agentName]: wts }))
    reconcileActiveWorktree(agentName, wts)
  }, [reconcileActiveWorktree])

  // Fetch each agent's worktrees whenever the agent list (or tunnel) changes,
  // so the sidebar nesting tracks worktrees created here or on another client.
  // Also prune stale entries for agents no longer in `containers`.
  useEffect(() => {
    if (tunnelPort == null) return
    const names = new Set(containers.map(c => c.name))
    setWorktrees(prev => {
      const stale = Object.keys(prev).filter(k => !names.has(k))
      return stale.length ? Object.fromEntries(Object.entries(prev).filter(([k]) => names.has(k))) : prev
    })
    const ac = new AbortController()
    for (const c of containers) {
      fetch(`http://127.0.0.1:${tunnelPort}/agents/${encodeURIComponent(c.name)}/worktrees`, { signal: ac.signal })
        .then(r => r.ok ? r.json() : Promise.reject(new Error('http')))
        .then((wts: WorktreeMeta[]) => applyWorktrees(c.name, wts))
        .catch(() => { /* transient; next push retries */ })
    }
    return () => ac.abort()
  }, [containers, tunnelPort, applyWorktrees])

  // Refresh one agent's worktree list on demand. Used at agent-chat turn
  // boundaries: a turn may have spawned or torn down a worktree server-side
  // (e.g. the agent ran a tool that removed one), and lair only pushes
  // `agents` on registry changes — never for worktree-only changes — so the
  // sidebar nesting would otherwise lag until the next unrelated agents push.
  const refetchWorktrees = useCallback((agentName: string) => {
    if (tunnelPort == null) return
    fetch(`http://127.0.0.1:${tunnelPort}/agents/${encodeURIComponent(agentName)}/worktrees`)
      .then(r => r.ok ? r.json() : Promise.reject(new Error('http')))
      .then((wts: WorktreeMeta[]) => applyWorktrees(agentName, wts))
      .catch(() => {})
  }, [tunnelPort, applyWorktrees])

  const createWorktree = useCallback((agentName: string, branch: string) => {
    const b = branch.trim()
    if (tunnelPort == null || !b) return
    fetch(`http://127.0.0.1:${tunnelPort}/agents/${encodeURIComponent(agentName)}/worktrees`, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ branch: b }),
    })
      .then(r => r.ok ? r.json() : Promise.reject(new Error('http')))
      .then((meta: WorktreeMeta) => {
        setWorktrees(prev => ({
          ...prev,
          [agentName]: [...(prev[agentName] ?? []).filter(w => w.id !== meta.id), meta],
        }))
        const c = containers.find(x => x.name === agentName)
        if (c) { setShowSidebar(false); sidebarAnim.setValue(0); openChatRef.current(c, meta) }
      })
      .catch(() => {})
  }, [tunnelPort, containers, sidebarAnim])

  const deleteWorktree = useCallback((agentName: string, wt: WorktreeMeta) => {
    if (tunnelPort == null) return
    // Optimistically drop the row, then fall back to the parent agent's chat
    // if we were viewing this worktree (same path as a worktree removed
    // elsewhere — see reconcileActiveWorktree).
    setWorktrees(prev => ({ ...prev, [agentName]: (prev[agentName] ?? []).filter(w => w.id !== wt.id) }))
    if (activeChildRef.current?.name === agentName && activeWorktreeRef.current?.id === wt.id) {
      openChatRef.current(activeChildRef.current)
    }
    fetch(`http://127.0.0.1:${tunnelPort}/agents/${encodeURIComponent(agentName)}/worktrees/${encodeURIComponent(wt.id)}`,
      { method: 'DELETE' })
      .then(() => fetch(`http://127.0.0.1:${tunnelPort}/agents/${encodeURIComponent(agentName)}/worktrees`))
      .then(r => r.ok ? r.json() : Promise.reject(new Error('http')))
      .then((wts: WorktreeMeta[]) => applyWorktrees(agentName, wts))
      .catch(() => {})
  }, [tunnelPort, applyWorktrees])

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

  const handleManualConnect = useCallback(() => {
    const raw = manualConn.trim()
    if (!raw) return
    Keyboard.dismiss()
    log(`[qr] manual connect raw=${raw}`)
    const parsed = parseQrData(raw)
    if (!parsed) {
      logE(`[qr] manual parse failed for: ${raw}`)
      setManualError('Invalid connect string')
      return
    }
    log(`[qr] manual host=${parsed.host} port=${parsed.port} pk=${parsed.pk.slice(0, 8)}…`)
    setManualError(null)
    AsyncStorage.setItem('masterConnection', JSON.stringify(parsed)).catch(() => {})
    setConn(parsed)
  }, [manualConn])

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
    clearAllCachedHistory()
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
            } else {
              // Drop the agent's cached history immediately. The lair-side
              // data dir is being removed too, so the next /history fetch
              // would 404; leaving stale rows in MMKV would resurrect a
              // ghost chat if the user later spawns a same-named agent.
              clearCachedHistory(`agent:${lairPk}:${c.name}`)
            }
          },
        },
      ],
    )
  }, [masterBaseUrl, lairPk])

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

  const openChild = useCallback((child: ContainerInfo, wt?: WorktreeMeta | null) => {
    const wtId = wt?.id ?? null
    // No-op if the sidebar tap targets the agent/worktree we're already viewing.
    if (childMounted && activeChild?.id === child.id && (activeWorktree?.id ?? null) === wtId) return
    if (childMounted && activeChild) {
      // Child → child: park the outgoing pane underneath so it covers the
      // master pane while `childSlideAnim` rewinds, then slide the new child
      // in from the right with the same animation used for master → child.
      setOutgoingChild(activeChild)
      setOutgoingWorktree(activeWorktree)
      setActiveChild(child)
      setActiveWorktree(wt ?? null)
      childSlideAnim.setValue(0)
      Animated.timing(childSlideAnim, { toValue: 1, duration: 260, useNativeDriver: true }).start(({ finished }) => {
        if (finished) { setOutgoingChild(null); setOutgoingWorktree(null) }
      })
    } else {
      setActiveChild(child)
      setActiveWorktree(wt ?? null)
      setChildMounted(true)
      childSlideAnim.setValue(0)
      Animated.timing(childSlideAnim, { toValue: 1, duration: 260, useNativeDriver: true }).start()
    }
  }, [childSlideAnim, childMounted, activeChild, activeWorktree])

  const closeChild = useCallback(() => {
    // If we're mid-swap, drop the outgoing pane up front — otherwise it would
    // be revealed as the active pane slides out.
    setOutgoingChild(null)
    setOutgoingWorktree(null)
    Animated.timing(childSlideAnim, { toValue: 0, duration: 220, useNativeDriver: true }).start(({ finished }) => {
      if (finished) {
        setActiveChild(null)
        setActiveWorktree(null)
        setChildMounted(false)
        setShowSidebar(false)
        sidebarAnim.setValue(0)
      }
    })
  }, [childSlideAnim, sidebarAnim])

  // Navigate to the main (LAIR) chat from the sidebar. Closes the sidebar
  // immediately, then slides the child pane out if one is showing.
  const goToMaster = useCallback(() => {
    if (childMounted) {
      setShowSidebar(false)
      sidebarAnim.setValue(0)
      closeChild()
    } else {
      closeSidebar()
    }
  }, [childMounted, closeChild, closeSidebar, sidebarAnim])

  // Keep the ref in sync so handleContainersUpdate can trigger animation.
  useEffect(() => { openChatRef.current = openChild }, [openChild])

  // ── QR scanner overlay ──────────────────────────────────────────────────────
  if (scanning) {
    return <QrScanner onScanned={handleQrScanned} onCancel={() => setScanning(false)} />
  }

  // ── Connection error screen ──────────────────────────────────────────────────
  if (conn && !tunnelPort && tunnelError) {
    return (
      <SafeAreaView style={{ flex: 1, backgroundColor: 'rgb(246,246,246)' }} edges={['top', 'bottom']}>
        <View flex={1} alignItems="center" justifyContent="center" gap={14} paddingHorizontal={40}>
          <AppIcon />
          <Text marginTop={20} paddingLeft={8} fontFamily="$heading" fontSize={40} fontWeight="800" letterSpacing={8} color="$typography900">OCTO</Text>
          <View marginVertical={6} height={2} width={36} borderRadius={999} backgroundColor="$primary600" opacity={0.7} />
          <Text overflow="hidden" borderRadius={12} backgroundColor="$error50" paddingHorizontal={14} paddingVertical={10} textAlign="center" fontFamily="$body" fontSize={13} lineHeight={19} color="$error600">{tunnelError}</Text>
          <Touchable marginTop={16} borderRadius={14} backgroundColor="$typography900" paddingHorizontal={40} paddingVertical={14} onPress={() => setConn(null)}>
            <Text fontFamily="$body" fontSize={12} fontWeight="700" textTransform="uppercase" letterSpacing={1.8} color="$typography0">back</Text>
          </Touchable>
        </View>
      </SafeAreaView>
    )
  }

  // ── No master connection yet ─────────────────────────────────────────────────
  if (!conn) {
    return (
      <SafeAreaView style={{ flex: 1, backgroundColor: 'rgb(246,246,246)' }} edges={['top', 'bottom']}>
        <Reanimated.View style={[{ flex: 1 }, setupKeyboardLift]}>
          <View flex={1} alignItems="center" justifyContent="center" gap={14} paddingHorizontal={40}>
            <Touchable testID="scan-trigger" onPress={requestCameraAndScan} activeOpacity={0.85}>
              <AppIcon pulse />
            </Touchable>
            <Text marginTop={20} paddingLeft={8} fontFamily="$heading" fontSize={40} fontWeight="800" letterSpacing={8} color="$typography900">OCTO</Text>
            <View marginVertical={6} height={2} width={36} borderRadius={999} backgroundColor="$primary600" opacity={0.7} />
            <Text fontSize={10.5} fontWeight="600" textTransform="uppercase" letterSpacing={2.4} color="$typography500" style={{ fontFamily: MONO }}>Distributed Coding Agents</Text>
            <Text marginTop={14} maxWidth={300} textAlign="center" fontFamily="$body" fontSize={14} lineHeight={22} color="$typography600">Tap the mark to scan your session QR code.</Text>
            <Text marginTop={20} fontSize={10.5} fontWeight="600" textTransform="uppercase" letterSpacing={2.4} color="$typography500" style={{ fontFamily: MONO }}>or paste a connect string</Text>
            <TextInput
              testID="manual-connect-input"
              style={{ fontFamily: MONO, marginTop: 6, width: '100%', maxWidth: 340, borderRadius: 14, borderWidth: 1, borderColor: 'rgb(221,220,219)', backgroundColor: '#fff', paddingHorizontal: 16, paddingVertical: 14, fontSize: 13.5, color: 'rgb(15,23,42)' }}
              value={manualConn}
              onChangeText={(t) => { setManualConn(t); if (manualError) setManualError(null) }}
              onSubmitEditing={handleManualConnect}
              placeholder="2:host:port:key"
              placeholderTextColor={C.textMuted}
              autoCapitalize="none"
              autoCorrect={false}
              autoComplete="off"
              spellCheck={false}
              returnKeyType="go"
            />
            {manualError ? <Text overflow="hidden" borderRadius={12} backgroundColor="$error50" paddingHorizontal={14} paddingVertical={10} textAlign="center" fontFamily="$body" fontSize={13} lineHeight={19} color="$error600">{manualError}</Text> : null}
            <Touchable
              testID="manual-connect-button"
              marginTop={16} borderRadius={14} paddingHorizontal={40} paddingVertical={14} backgroundColor="$typography900" opacity={!manualConn.trim() ? 0.35 : undefined}
              onPress={handleManualConnect}
              disabled={!manualConn.trim()}
            >
              <Text fontFamily="$body" fontSize={12} fontWeight="700" textTransform="uppercase" letterSpacing={1.8} color="$typography0">connect</Text>
            </Touchable>
          </View>
        </Reanimated.View>
      </SafeAreaView>
    )
  }


  // ── Master + child overlay layout ───────────────────────────────────────────
  const masterTranslateX = childSlideAnim.interpolate({
    inputRange: [0, 1],
    outputRange: [0, -(screenW * 0.3)],
  })
  const childTranslateX = childSlideAnim.interpolate({
    inputRange: [0, 1],
    outputRange: [screenW, 0],
  })

  // ── Master chat UI ───────────────────────────────────────────────────────────
  return (
    <SafeAreaView style={{ flex: 1, backgroundColor: 'rgb(246,246,246)' }} edges={['top']}>
      <View flex={1}>
        <Animated.View style={[{ flex: 1, transform: [{ translateX: masterTranslateX }] }]}>
          <View flexDirection="row" alignItems="center" justifyContent="space-between" borderBottomWidth={1} borderColor="$outline200" backgroundColor="$background50" paddingHorizontal={16} paddingVertical={12}>
            <View flex={1} flexDirection="row" alignItems="center" gap={10}>
              <Touchable
                testID="open-sidebar"
                marginRight={4} paddingHorizontal={6} paddingVertical={8}
                onPress={openSidebar}
                hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
              >
                <View height={12} width={18} justifyContent="space-between">
                  <View height={2} borderRadius={999} backgroundColor="$typography900" />
                  <View height={2} borderRadius={999} backgroundColor="$typography900" />
                  <View height={2} borderRadius={999} backgroundColor="$typography900" />
                </View>
              </Touchable>
            </View>
            <View flexDirection="row" alignItems="center" gap={8}>
              <TasksHeaderButton tasks={masterTasks} onPress={() => { Keyboard.dismiss(); setShowTasksModal(true) }} />
              <Touchable
                borderRadius={999} borderWidth={1} borderColor="$outline200" backgroundColor="$background0" paddingHorizontal={12} paddingVertical={5}
                onPress={() => clearChatRef.current()}
                hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
                disabled={chatStatus !== 'ready'}
              >
                <Text fontFamily="$body" fontSize={11} fontWeight="600" letterSpacing={0.4} color="$typography600" opacity={chatStatus !== 'ready' ? 0.3 : undefined}>clear</Text>
              </Touchable>
            </View>
          </View>

          {masterBaseUrl && (
            <ChatPane
              baseUrl={masterBaseUrl}
              cacheKey={masterCacheKey}
              onStatusChange={setChatStatus}
              clearRef={clearChatRef}
              initialDraft={draftsRef.current['master']}
              onDraftChange={d => { draftsRef.current['master'] = d }}
              reconnectingRef={reconnectingRef}
              reloadRef={reloadRef}
              closeWsRef={closeWsRef}
              sendFrameRef={masterSendFrameRef}
              onContainersUpdate={handleContainersUpdate}
              onTasksUpdate={handleMasterTasksUpdate}
              onCancelAck={masterHandleCancelAck}
            />
          )}

          <TasksModal
            visible={showTasksModal}
            tasks={masterTasks}
            cancellingIds={masterCancellingIds}
            onClose={() => setShowTasksModal(false)}
            onCancel={(id) => {
              log(`[app] cancel_task id=${id} (master)`)
              masterRequestCancel(id)
            }}
          />

          <ConnectionErrorModal
            visible={chatStatus === 'error' && !dismissedConnError}
            onDismiss={() => setDismissedConnError(true)}
          />
        </Animated.View>

        {outgoingChild && (
          // Static cover for the master pane during a child → child swap.
          // Unmounts once `openChild` finishes the slide-in. Pointer events
          // disabled so taps fall through to the incoming pane on top.
          <View style={StyleSheet.absoluteFillObject} pointerEvents="none">
            <ChildChatScreen
              key={`outgoing-${outgoingChild.id}${outgoingWorktree ? `::${outgoingWorktree.id}` : ''}`}
              child={outgoingChild}
              worktree={outgoingWorktree}
              tunnelPort={tunnelPort}
              tunnelError={tunnelError}
              cacheKey={outgoingWorktree
                ? `worktree:${lairPk}:${outgoingChild.name}:${outgoingWorktree.id}`
                : `agent:${lairPk}:${outgoingChild.name}`}
              onOpenSidebar={openSidebar}
              initialDraft={draftsRef.current[`${outgoingChild.id}${outgoingWorktree ? `::${outgoingWorktree.id}` : ''}`]}
              onDraftChange={d => { draftsRef.current[`${outgoingChild.id}${outgoingWorktree ? `::${outgoingWorktree.id}` : ''}`] = d }}
              reconnectingRef={reconnectingRef}
            />
          </View>
        )}

        {childMounted && activeChild && (
          <Animated.View style={[StyleSheet.absoluteFillObject, { transform: [{ translateX: childTranslateX }] }]}>
            <ChildChatScreen
              key={`${activeChild.id}${activeWorktree ? `::${activeWorktree.id}` : ''}`}
              child={activeChild}
              worktree={activeWorktree}
              tunnelPort={tunnelPort}
              tunnelError={tunnelError}
              cacheKey={activeWorktree
                ? `worktree:${lairPk}:${activeChild.name}:${activeWorktree.id}`
                : `agent:${lairPk}:${activeChild.name}`}
              onOpenSidebar={openSidebar}
              initialDraft={draftsRef.current[`${activeChild.id}${activeWorktree ? `::${activeWorktree.id}` : ''}`]}
              onDraftChange={d => { draftsRef.current[`${activeChild.id}${activeWorktree ? `::${activeWorktree.id}` : ''}`] = d }}
              reconnectingRef={reconnectingRef}
              reloadRef={reloadRef}
              closeWsRef={closeWsRef}
              onTurnEnd={refetchWorktrees}
            />
          </Animated.View>
        )}

        {startingContainerId !== null && (
          <View position="absolute" top={0} right={0} bottom={0} left={0} alignItems="center" justifyContent="center" gap={18} backgroundColor="$background50" paddingHorizontal={32}>
            {startingError ? (
              <>
                <Text textAlign="center" fontFamily="$body" fontSize={13} fontWeight="700" textTransform="uppercase" letterSpacing={1.6} color="$error600">Failed to start container</Text>
                <Text textAlign="center" fontFamily="$body" fontSize={13} lineHeight={19} color="$typography600">{startingError}</Text>
              </>
            ) : (
              <>
                <Spinner size="large" color={C.accent} />
                <Text fontSize={11} fontWeight="600" textTransform="uppercase" letterSpacing={1.8} color="$typography600" style={{ fontFamily: MONO }}>Starting container...</Text>
              </>
            )}
            <Touchable
              marginTop={10} borderRadius={12} borderWidth={1} borderColor="$outline200" backgroundColor="$background0" paddingHorizontal={32} paddingVertical={12}
              onPress={() => {
                startingContainerIdRef.current = null
                setStartingContainerId(null)
                setStartingError(null)
              }}
            >
              <Text fontFamily="$body" fontSize={12} fontWeight="700" textTransform="uppercase" letterSpacing={1.8} color="$typography900">{startingError ? 'dismiss' : 'cancel'}</Text>
            </Touchable>
          </View>
        )}
        {showSidebar && (
          <>
            <Animated.View
              style={[StyleSheet.absoluteFillObject, { zIndex: 200, backgroundColor: 'rgba(2,6,23,0.4)', opacity: sidebarAnim.interpolate({ inputRange: [0, 1], outputRange: [0, 1] }) }]}
              pointerEvents="box-none"
            >
              <Touchable
                style={StyleSheet.absoluteFillObject}
                activeOpacity={1}
                onPress={closeSidebar}
              />
            </Animated.View>
            <Animated.View
              style={{ position: 'absolute', bottom: 0, left: 0, top: 0, zIndex: 201, width: 308, flexDirection: 'column', overflow: 'hidden', borderTopRightRadius: 22, borderBottomRightRadius: 22, backgroundColor: 'rgb(246,246,246)', transform: [{ translateX: sidebarAnim.interpolate({ inputRange: [0, 1], outputRange: [-280, 0] }) }] }}
            >
              <View flexDirection="row" alignItems="center" justifyContent="space-between" borderBottomWidth={1} borderColor="$outline200" paddingHorizontal={20} paddingVertical={20}>
                <View>
                  <Text paddingLeft={5} fontFamily="$heading" fontSize={22} fontWeight="800" letterSpacing={5} color="$typography900">OCTO</Text>
                  <Text marginTop={6} fontSize={10} fontWeight="600" textTransform="uppercase" letterSpacing={1.8} color="$typography500" style={{ fontFamily: MONO }}>Agent Console</Text>
                </View>
                <Touchable onPress={closeSidebar} hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}>
                  <Text fontSize={16} fontWeight="300" color="$typography600">✕</Text>
                </Touchable>
              </View>
              <ScrollView
                style={{ flex: 1 }}
                bounces={false}
                keyboardShouldPersistTaps="handled"
                showsVerticalScrollIndicator={false}
              >
                <Touchable
                  flexDirection="row" alignItems="center" gap={12} borderBottomWidth={1} borderColor="$outline200" paddingVertical={14} borderLeftWidth={!childMounted ? 3 : undefined} borderLeftColor={!childMounted ? '$primary600' : undefined} backgroundColor={!childMounted ? '$primary50' : undefined} paddingLeft={!childMounted ? 17 : 20} paddingRight={20}
                  onPress={goToMaster}
                  activeOpacity={0.7}
                >
                  <View height={8} width={8} borderRadius={999} style={{ backgroundColor: C.green }} />
                  <View flex={1}>
                    <Text fontFamily="$body" fontSize={14.5} fontWeight="600" color="$typography900">LAIR</Text>
                  </View>
                  <Text fontSize={10} fontWeight="700" textTransform="uppercase" letterSpacing={1.2} color="$typography500" style={{ fontFamily: MONO }}>main</Text>
                </Touchable>

                <View paddingHorizontal={20} paddingBottom={8} paddingTop={20}>
                  <Text fontSize={10.5} fontWeight="700" textTransform="uppercase" letterSpacing={1.6} color="$typography500" style={{ fontFamily: MONO }}>Agents</Text>
                </View>

                {containers.length === 0 && (
                  <View flexDirection="row" alignItems="center" gap={12} borderBottomWidth={1} borderColor="$outline200" paddingHorizontal={20} paddingVertical={14}>
                    <Text fontSize={10} fontWeight="700" textTransform="uppercase" letterSpacing={1.2} color="$typography500" style={{ fontFamily: MONO }}>No agents</Text>
                  </View>
                )}
                {containers.map(c => {
                  const active = childMounted && activeChild?.id === c.id && !activeWorktree
                  const cWorktrees = worktrees[c.name] ?? []
                  return (
                  <React.Fragment key={c.id}>
                  <Touchable
                    flexDirection="row" alignItems="center" gap={12} borderBottomWidth={1} borderColor="$outline200" paddingVertical={14} borderLeftWidth={active ? 3 : undefined} borderLeftColor={active ? '$primary600' : undefined} backgroundColor={active ? '$primary50' : undefined} paddingLeft={active ? 17 : 20} paddingRight={20}
                    onPress={() => {
                      if (c.status === 'running') {
                        setShowSidebar(false)
                        sidebarAnim.setValue(0)
                        openChild(c)
                      } else {
                        startContainer(c.id)
                      }
                    }}
                    onLongPress={() => terminateAgent(c)}
                    delayLongPress={500}
                    activeOpacity={0.7}
                  >
                    <View height={8} width={8} borderRadius={999} style={{ backgroundColor: c.status === 'running' ? C.green : C.textMuted }} />
                    <View flex={1}>
                      <Text fontFamily="$body" fontSize={14.5} fontWeight="600" color="$typography900">{containerDisplayName(c.name)}</Text>
                      {c.kind === 'remote' ? <Text marginTop={2} fontSize={11.5} color="$typography500" style={{ fontFamily: MONO }} numberOfLines={1}>remote</Text> : null}
                    </View>
                    {c.status === 'running' && (
                      <Touchable
                        hitSlop={{ top: 10, bottom: 10, left: 10, right: 10 }}
                        onPress={() => { setCreatingWtFor(prev => prev === c.name ? null : c.name); setNewBranchDraft('') }}
                      >
                        <Text paddingHorizontal={4} fontSize={18} fontWeight="600" color="$typography500">＋</Text>
                      </Touchable>
                    )}
                  </Touchable>

                  {cWorktrees.map(wt => {
                    const wtActive = childMounted && activeChild?.id === c.id && activeWorktree?.id === wt.id
                    return (
                      <Touchable
                        key={`${c.id}::${wt.id}`}
                        flexDirection="row" alignItems="center" gap={10} borderBottomWidth={1} borderColor="$outline200" backgroundColor={wtActive ? '$primary50' : '$background0'} paddingVertical={14} paddingLeft={34} borderLeftWidth={wtActive ? 3 : undefined} borderLeftColor={wtActive ? '$primary600' : undefined} paddingRight={20}
                        onPress={() => { setShowSidebar(false); sidebarAnim.setValue(0); openChild(c, wt) }}
                        onLongPress={() => Alert.alert(
                          'Delete worktree?',
                          `Removes the "${wt.branch}" worktree and deletes its branch. This cannot be undone.`,
                          [
                            { text: 'Cancel', style: 'cancel' },
                            { text: 'Delete', style: 'destructive', onPress: () => deleteWorktree(c.name, wt) },
                          ],
                        )}
                        delayLongPress={500}
                        activeOpacity={0.7}
                      >
                        <Text width={12} fontSize={13} fontWeight="700" color="$typography500">⌥</Text>
                        <View flex={1}>
                          <Text fontFamily="$body" fontSize={13.5} fontWeight="500" color="$typography600" numberOfLines={1}>{wt.branch}</Text>
                        </View>
                        <Text fontSize={10} fontWeight="700" textTransform="uppercase" letterSpacing={1.2} color="$typography500" style={{ fontFamily: MONO }}>worktree</Text>
                      </Touchable>
                    )
                  })}

                  {creatingWtFor === c.name && (
                    <View flexDirection="row" alignItems="center" gap={10} borderBottomWidth={1} borderColor="$outline200" backgroundColor="$background0" paddingVertical={14} paddingLeft={34} paddingRight={20}>
                      <TextInput
                        style={{ fontFamily: MONO, flex: 1, borderRadius: 6, borderWidth: 1, borderColor: 'rgb(221,220,219)', backgroundColor: 'rgb(246,246,246)', paddingHorizontal: 8, paddingVertical: 4, fontSize: 13, color: 'rgb(15,23,42)' }}
                        autoFocus
                        placeholder="new branch name…"
                        placeholderTextColor={C.textMuted}
                        value={newBranchDraft}
                        onChangeText={setNewBranchDraft}
                        autoCapitalize="none"
                        autoCorrect={false}
                        returnKeyType="done"
                        onSubmitEditing={() => { createWorktree(c.name, newBranchDraft); setCreatingWtFor(null); setNewBranchDraft('') }}
                        onBlur={() => { setCreatingWtFor(null); setNewBranchDraft('') }}
                      />
                    </View>
                  )}
                  </React.Fragment>
                  )
                })}
              </ScrollView>
              <View height={1} backgroundColor="$outline200" />
              <Touchable paddingHorizontal={20} paddingVertical={16} onPress={handleLogout}>
                <Text fontFamily="$body" fontSize={13} fontWeight="700" letterSpacing={0.4} color="$error600">exit</Text>
              </Touchable>
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
      <OktoProvider mode="light">
        <KeyboardProvider>
          <SafeAreaProvider>
            <AppInner />
          </SafeAreaProvider>
        </KeyboardProvider>
      </OktoProvider>
    </ErrorBoundary>
  )
}

// ── Styles ────────────────────────────────────────────────────────────────────

const s = StyleSheet.create({
  // Only the styles that remain hard to express as utility classes:
  // markdown text rendering (renderText), the dark QR-scanner overlay, and the
  // geometric paper-plane / orbiting-arc marks. Everything else moved to
  // NativeWind className utilities driven by the shared @okto/ui tokens.

  // ── Markdown text (renderText) ───────────────────────────────────────────────
  textBlock:          { color: '#FFFFFF', fontSize: 15.5, lineHeight: 23, fontWeight: '400', fontFamily: ARIMO },
  assistantTextBlock: { color: C.textPrimary, fontSize: 15.5, lineHeight: 24, fontWeight: '400', fontFamily: ARIMO },
  inlineCode:         { fontFamily: MONO, fontSize: 12.5, color: C.textPrimary, backgroundColor: C.surface, paddingHorizontal: 5, paddingVertical: 1, borderRadius: 4, borderWidth: StyleSheet.hairlineWidth, borderColor: C.border },
  codeBlock:          { backgroundColor: C.surfaceSoft, borderRadius: 12, padding: 14, marginVertical: 8, borderWidth: StyleSheet.hairlineWidth, borderColor: C.border },
  codeBlockText:      { fontFamily: MONO, fontSize: 12.5, color: C.textPrimary, lineHeight: 19 },
  codeBlockLang:      { fontSize: 10, color: C.textMuted, fontFamily: MONO, marginBottom: 8, textTransform: 'uppercase', letterSpacing: 1.2, fontWeight: '600' },

  // ── App icon mark ────────────────────────────────────────────────────────────
  creatureImg:        { width: 116, height: 116, borderRadius: 28, marginBottom: 10 },

  // ── Orbiting arc (streaming stop button) ─────────────────────────────────────
  orbitArc:        { position: 'absolute', borderColor: 'transparent', borderTopColor: C.accent },

  // ── QR scanner (intentionally a dark, high-contrast camera surface) ──────────
  scannerFull:       { ...StyleSheet.absoluteFillObject, backgroundColor: '#0A0E12', zIndex: 100 },
  scannerOverlay:    { ...StyleSheet.absoluteFillObject, alignItems: 'center', justifyContent: 'space-between', paddingVertical: 80 },
  scannerTopBar:     { alignItems: 'center', gap: 6, paddingHorizontal: 32 },
  scannerIcon:       { width: 56, height: 56, borderRadius: 16, marginBottom: 6 },
  scannerTitle:      { color: '#F4EFE3', fontSize: 13, fontWeight: '700', fontFamily: ARIMO, letterSpacing: 2.4, textTransform: 'uppercase' },
  scannerSubtitle:   { color: 'rgba(244,239,227,0.62)', fontSize: 12, textAlign: 'center', lineHeight: 18, fontFamily: ARIMO, marginTop: 6 },
  scannerReticle:    { width: 240, height: 240 },
  scannerCorner:     { position: 'absolute', width: 32, height: 32, borderColor: '#F4EFE3', borderWidth: 2.5, borderRadius: 4 },
  cornerTL:          { top: 0, left: 0, borderRightWidth: 0, borderBottomWidth: 0, borderTopLeftRadius: 12 },
  cornerTR:          { top: 0, right: 0, borderLeftWidth: 0, borderBottomWidth: 0, borderTopRightRadius: 12 },
  cornerBL:          { bottom: 0, left: 0, borderRightWidth: 0, borderTopWidth: 0, borderBottomLeftRadius: 12 },
  cornerBR:          { bottom: 0, right: 0, borderLeftWidth: 0, borderTopWidth: 0, borderBottomRightRadius: 12 },
  scannerCancel:     { borderWidth: 1, borderColor: 'rgba(244,239,227,0.45)', borderRadius: 999, paddingVertical: 12, paddingHorizontal: 40 },
  scannerCancelText: { color: '#F4EFE3', fontSize: 12, fontWeight: '700', fontFamily: ARIMO, letterSpacing: 1.8, textTransform: 'uppercase' },
  scannerError:      { color: '#FF9A8A', fontSize: 13, textAlign: 'center', marginBottom: 24, fontFamily: ARIMO, letterSpacing: 0.2 },
})
