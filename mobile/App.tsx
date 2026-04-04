import AsyncStorage from '@react-native-async-storage/async-storage'
import React, { useCallback, useEffect, memo, useRef, useState } from 'react'
import {
  ActivityIndicator,
  Animated,
  AppState,
  FlatList,
  PermissionsAndroid,
  Platform,
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

type ServerFrame =
  | { type: 'history';  messages: HistMsg[] }
  | { type: 'token';    text: string }
  | { type: 'tool';     name: string }
  | { type: 'question'; question: string }
  | { type: 'done'; cost_usd: number }
  | { type: 'error';    message: string }

interface HistMsg { role: 'user' | 'assistant'; text: string }

interface Message {
  id:          string
  role:        'user' | 'assistant'
  text:        string
  streaming?:  boolean
  isQuestion?: boolean
  cost?:       number
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

function renderBoldText(text: string, baseStyle: object) {
  const parts = text.split(/\*\*(.+?)\*\*/gs)
  if (parts.length === 1) return <Text style={baseStyle}>{text}</Text>
  return (
    <Text style={baseStyle}>
      {parts.map((part, i) =>
        i % 2 === 1
          ? <Text key={i} style={{ fontWeight: '900' }}>{part}</Text>
          : part
      )}
    </Text>
  )
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

const MessageBubble = memo(function MessageBubble({ message }: { message: Message }) {
  const isUser = message.role === 'user'
  return (
    <View style={[s.messageWrap, isUser && s.messageWrapRight]}>
      <Text style={[s.messageLabel, isUser && s.messageLabelRight]}>
        {isUser ? 'you' : 'claude'}
      </Text>
      {message.isQuestion && <Text style={s.questionMark}>?</Text>}
      {renderBoldText(message.text, s.textBlock)}
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

const ChatPane = memo(function ChatPane({
  wsUrl, connKey, onStatusChange, clearRef, interruptRef,
}: {
  wsUrl:          string
  connKey:        string
  onStatusChange: (s: ConnStatus) => void
  clearRef:       React.MutableRefObject<() => void>
  interruptRef:   React.MutableRefObject<() => void>
}) {
  const insets                     = useSafeAreaInsets()
  const { height: keyboardHeight } = useReanimatedKeyboardAnimation()
  const spacerStyle                = useAnimatedStyle(() => ({
    height: Math.max(insets.bottom, -keyboardHeight.value),
  }))

  const [messages,       setMessages]       = useState<Message[]>([])
  const [status,         setStatus]         = useState<ConnStatus>('connecting')
  const [input,          setInput]          = useState('')
  const [inputHeight,    setInputHeight]    = useState(48)
  const [pendingQuestion, setPendingQuestion] = useState(false)

  const wsRef           = useRef<WebSocket | null>(null)
  const sendMessageRef  = useRef<() => void>(() => {})
  const listRef         = useRef<FlatList<Message>>(null)
  const isAtBottomRef   = useRef(true)

  const updateStatus = useCallback((s: ConnStatus) => {
    setStatus(s)
    onStatusChange(s)
  }, [onStatusChange])

  // WebSocket connection lifecycle
  useEffect(() => {
    let cancelled = false
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null

    // Show cached messages while connecting
    AsyncStorage.getItem(`msgs_${connKey}`)
      .then(json => { if (json && !cancelled) try { setMessages(JSON.parse(json)) } catch {} })
      .catch(() => {})

    const connect = () => {
      if (cancelled) return
      console.log(`[ws] connecting to ${wsUrl}`)
      updateStatus('connecting')
      const ws = new WebSocket(wsUrl)
      wsRef.current = ws

      ws.onopen = () => console.log('[ws] opened')

      ws.onmessage = ({ data }: { data: string }) => {
        if (cancelled) return
        let frame: ServerFrame
        try { frame = JSON.parse(data as string) } catch { return }

        switch (frame.type) {
          case 'history': {
            const msgs: Message[] = frame.messages.map((m, i) => ({
              id: `h${i}`, role: m.role, text: m.text,
            }))
            setMessages(msgs)
            setPendingQuestion(false)
            isAtBottomRef.current = true
            updateStatus('ready')
            AsyncStorage.setItem(`msgs_${connKey}`, JSON.stringify(msgs)).catch(() => {})
            break
          }
          case 'token': {
            updateStatus('streaming')
            setMessages(prev => {
              const last = prev[prev.length - 1]
              if (last?.streaming && last.role === 'assistant') {
                return [...prev.slice(0, -1), { ...last, text: last.text + frame.text }]
              }
              return [...prev, { id: uid(), role: 'assistant' as const, text: frame.text, streaming: true }]
            })
            break
          }
          case 'done': {
            const cost = frame.cost_usd
            setMessages(prev => prev.map((m, i) => {
              const isLast = i === prev.length - 1
              const updates: Partial<Message> = {}
              if (m.streaming)                            updates.streaming = false
              if (cost > 0 && isLast && m.role === 'assistant') updates.cost = cost
              return Object.keys(updates).length ? { ...m, ...updates } : m
            }))
            updateStatus('ready')
            setPendingQuestion(false)
            break
          }
          case 'question': {
            setMessages(prev => {
              const finalized = prev.map(m => m.streaming ? { ...m, streaming: false } : m)
              return [...finalized, { id: uid(), role: 'assistant' as const, text: frame.question, isQuestion: true }]
            })
            setPendingQuestion(true)
            updateStatus('ready')
            break
          }
          case 'error': {
            setMessages(prev => {
              const last = prev[prev.length - 1]
              if (last?.streaming) {
                return [...prev.slice(0, -1), { ...last, streaming: false }]
              }
              return [...prev, { id: uid(), role: 'assistant' as const, text: `✗ ${frame.message}` }]
            })
            updateStatus('ready')
            break
          }
        }
      }

      ws.onerror = (e: Event) => {
        if (cancelled) return
        console.error('[ws] error', e)
        updateStatus('error')
      }

      ws.onclose = (e: CloseEvent) => {
        if (cancelled) return
        console.log(`[ws] closed code=${e.code} reason=${e.reason}`)
        updateStatus('connecting')
        reconnectTimer = setTimeout(connect, 1500)
      }
    }

    connect()

    return () => {
      cancelled = true
      if (reconnectTimer) clearTimeout(reconnectTimer)
      wsRef.current?.close()
    }
  }, [wsUrl, connKey])

  // When app foregrounds: force-close WS so onclose fires and we reconnect.
  // (Noise tunnel re-establishment is handled by AppInner.)
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

  const sendMessage = useCallback(() => {
    const text = input.trim()
    if (!text || status === 'streaming') return
    const ws = wsRef.current
    if (!ws || ws.readyState !== WebSocket.OPEN) return

    setMessages(prev => [...prev, { id: uid(), role: 'user' as const, text }])
    isAtBottomRef.current = true

    if (pendingQuestion) {
      ws.send(JSON.stringify({ type: 'answer', answer: text }))
      setPendingQuestion(false)
    } else {
      ws.send(JSON.stringify({ type: 'message', text }))
    }
    updateStatus('streaming')

    setInput('')
    setInputHeight(48)
  }, [input, pendingQuestion, status])

  sendMessageRef.current = sendMessage

  const clearConversation = useCallback(() => {
    wsRef.current?.send(JSON.stringify({ type: 'clear' }))
    setMessages([])
    setPendingQuestion(false)
    AsyncStorage.removeItem(`msgs_${connKey}`).catch(() => {})
  }, [connKey])
  clearRef.current = clearConversation

  interruptRef.current = () => {
    wsRef.current?.send(JSON.stringify({ type: 'interrupt' }))
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
          onLayout={() => {
            if (isAtBottomRef.current) {
              listRef.current?.scrollToEnd({ animated: false })
            }
          }}
          onContentSizeChange={() => {
            if (isAtBottomRef.current) {
              listRef.current?.scrollToEnd({ animated: true })
            }
          }}
          onScroll={({ nativeEvent: { layoutMeasurement, contentOffset, contentSize } }) => {
            isAtBottomRef.current = contentOffset.y + layoutMeasurement.height >= contentSize.height - 40
          }}
          scrollEventThrottle={100}
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

        <View style={{ backgroundColor: C.surface }}>
          <View style={s.inputRow}>
            <TextInput
              style={[s.input, { height: inputHeight }]}
              value={input}
              onChangeText={text => {
                if (text.includes('\n')) { sendMessageRef.current(); return }
                setInput(text)
              }}
              onContentSizeChange={e =>
                setInputHeight(Math.min(e.nativeEvent.contentSize.height, 140))
              }
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
      </View>
      {/* Spacer whose height matches the keyboard height (or bottom safe area when
          keyboard is hidden). Growing this spacer shrinks the flex:1 content above,
          so the entire conversation+input block moves up with the keyboard. */}
      <Reanimated.View style={[{ backgroundColor: C.surface }, spacerStyle]} />
    </View>
  )
})

// ── ConnRow ────────────────────────────────────────────────────────────────────

function ConnRow({ conn, onSelect, onDelete }: {
  conn:     NoiseConnectionInfo
  onSelect: () => void
  onDelete: () => void
}) {
  return (
    <TouchableOpacity style={s.connRow} onPress={onSelect} activeOpacity={0.7}>
      <View style={s.connInfo}>
        <Text style={s.connLabel}>{conn.label ?? conn.host}</Text>
        <Text style={s.connHost}>{conn.host}:{conn.port}</Text>
      </View>
      <TouchableOpacity onPress={onDelete} hitSlop={{ top: 12, bottom: 12, left: 12, right: 12 }}>
        <Text style={s.connDelete}>×</Text>
      </TouchableOpacity>
    </TouchableOpacity>
  )
}

// ── AppInner ──────────────────────────────────────────────────────────────────

function AppInner() {
  const [conn,        setConn]        = useState<NoiseConnectionInfo | null>(null)
  const [tunnelPort,  setTunnelPort]  = useState<number | null>(null)
  const [tunnelError, setTunnelError] = useState<string | null>(null)
  const [scanning,    setScanning]    = useState(false)
  const [savedConns,  setSavedConns]  = useState<NoiseConnectionInfo[]>([])
  const [repoName,    setRepoName]    = useState<string | null>(null)
  const [chatStatus,  setChatStatus]  = useState<ConnStatus>('connecting')
  const clearChatRef     = useRef<() => void>(() => {})
  const interruptChatRef = useRef<() => void>(() => {})

  // Load saved connections on mount; auto-connect if exactly one saved.
  useEffect(() => {
    let cancelled = false
    AsyncStorage.getItem('noiseConnections').then(json => {
      if (cancelled) return
      let conns: NoiseConnectionInfo[] = []
      if (json) { try { conns = JSON.parse(json) } catch {} }
      // In dev builds, always ensure the local dev connection is listed first.
      if (__DEV__) {
        const devKey = connKeyFor(DEV_CONN)
        if (!conns.some(c => connKeyFor(c) === devKey)) {
          conns = [DEV_CONN, ...conns]
        }
      }
      setSavedConns(conns)
      if (conns.length === 1) setConn(conns[0])
    })
    return () => { cancelled = true }
  }, [])

  // Establish Noise tunnel when conn changes.
  useEffect(() => {
    setTunnelPort(null)
    setTunnelError(null)
    setRepoName(null)
    if (!conn) return
    let connected = false
    const timer = setTimeout(() => {
      console.log(`[noise] connecting to ${conn.host}:${conn.port} pk=${conn.pk.slice(0, 8)}…`)
      NoiseConnection.connect(conn.host, conn.port, conn.pk)
        .then(port => {
          console.log(`[noise] tunnel established → local port ${port}`)
          connected = true
          setTunnelPort(port)
        })
        .catch(e => {
          const msg = e?.message ?? String(e)
          console.error(`[noise] connect failed: ${msg}`)
          setTunnelError(msg)
        })
    }, 50)
    return () => {
      clearTimeout(timer)
      if (connected) NoiseConnection.disconnect()
    }
  }, [conn])

  // Re-establish Noise tunnel when app returns to foreground (iOS kills the
  // native TCP proxy during suspension; without this the WS reconnect silently fails).
  useEffect(() => {
    if (!conn) return
    const sub = AppState.addEventListener('change', nextState => {
      if (nextState === 'active') {
        console.log('[noise] app foregrounded — re-establishing tunnel')
        NoiseConnection.disconnect()
        NoiseConnection.connect(conn.host, conn.port, conn.pk)
          .then(port => {
            console.log(`[noise] tunnel re-established → local port ${port}`)
            setTunnelPort(port)
          })
          .catch(e => console.error(`[noise] reconnect failed: ${e?.message ?? e}`))
      }
    })
    return () => sub.remove()
  }, [conn])

  // Fetch repo name once tunnel is up.
  useEffect(() => {
    if (!tunnelPort || !conn) return
    fetch(`http://127.0.0.1:${tunnelPort}/config`)
      .then(r => r.ok ? r.json() : null)
      .then((d: { name?: string | null } | null) => {
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

  const handleQrScanned = useCallback((raw: string) => {
    setScanning(false)
    const parsed = parseQrData(raw)
    if (!parsed) { setTunnelError('Invalid QR code'); return }
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
      if (granted !== PermissionsAndroid.RESULTS.GRANTED) return
    }
    setScanning(true)
  }, [])

  const deleteConn = useCallback((c: NoiseConnectionInfo) => {
    setSavedConns(prev => {
      const updated = prev.filter(x => !(x.host === c.host && x.port === c.port))
      AsyncStorage.setItem('noiseConnections', JSON.stringify(updated))
      return updated
    })
    if (conn?.host === c.host && conn?.port === c.port) setConn(null)
  }, [conn])

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

  // ── Connection picker (no conn selected) ────────────────────────────────────
  if (!conn) {
    return (
      <SafeAreaView style={s.setupSafe} edges={['top', 'bottom']}>
        {savedConns.length === 0 ? (
          <View style={s.setupCenter}>
            <CreatureAnim />
            <Text style={s.setupTitle}>claudulhu</Text>
            <Text style={s.setupDesc}>Connect to your Claude Code server</Text>
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
            <View style={{ flex: 1 }}>
              {savedConns.map(c => (
                <ConnRow key={connKeyFor(c)} conn={c} onSelect={() => setConn(c)} onDelete={() => deleteConn(c)} />
              ))}
            </View>
            <View style={s.pickerFooter}>
              <TouchableOpacity style={s.setupBtn} onPress={requestCameraAndScan}>
                <Text style={s.setupBtnText}>Scan QR code</Text>
              </TouchableOpacity>
            </View>
          </View>
        )}
      </SafeAreaView>
    )
  }

  // ── Chat UI ─────────────────────────────────────────────────────────────────
  return (
    <SafeAreaView style={s.safe} edges={['top']}>
      <View style={s.paneArea}>
        {/* Header */}
        <View style={s.header}>
          <View style={s.headerLeft}>
            <TouchableOpacity
              style={s.backBtn}
              onPress={() => setConn(null)}
              hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
            >
              <Text style={s.backBtnText}>‹</Text>
            </TouchableOpacity>
            <View style={[s.connDot, { backgroundColor: statusColor(chatStatus) }]} />
            <View>
              <Text style={s.headerTitle}>claudulhu</Text>
              {repoName && <Text style={s.headerRepo}>{repoName}</Text>}
            </View>
          </View>
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
        </View>

        <ChatPane
          wsUrl={`ws://127.0.0.1:${tunnelPort}/chat`}
          connKey={connKeyFor(conn)}
          onStatusChange={setChatStatus}
          clearRef={clearChatRef}
          interruptRef={interruptChatRef}
        />
      </View>
    </SafeAreaView>
  )
}

// ── App ────────────────────────────────────────────────────────────────────────

export default function App() {
  return (
    <KeyboardProvider>
      <SafeAreaProvider>
        <AppInner />
      </SafeAreaProvider>
    </KeyboardProvider>
  )
}

// ── Styles ────────────────────────────────────────────────────────────────────

const s = StyleSheet.create({
  // Setup / connecting / picker
  setupSafe:    { flex: 1, backgroundColor: '#EB4F0B' },
  setupCenter:  { flex: 1, alignItems: 'center', justifyContent: 'center', paddingHorizontal: 40, gap: 16 },
  setupTitle:   { fontSize: 26, fontWeight: '700', color: '#fff', letterSpacing: 2 },
  setupDesc:    { fontSize: 15, color: 'rgba(255,255,255,0.85)', textAlign: 'center', lineHeight: 22 },
  setupStatus:  { fontSize: 15, color: 'rgba(255,255,255,0.7)', textAlign: 'center' },
  setupError:   { fontSize: 14, color: '#ffe0d6', textAlign: 'center', lineHeight: 20 },
  setupBtn:     { backgroundColor: '#fff', borderRadius: 12, paddingVertical: 14, paddingHorizontal: 32, alignItems: 'center', marginTop: 8 },
  setupBtnText: { color: '#EB4F0B', fontWeight: '700', fontSize: 16 },
  pickerWrap:   { flex: 1, backgroundColor: '#EB4F0B' },
  pickerHeader: { alignItems: 'center', paddingTop: 48, paddingBottom: 24, gap: 8 },
  pickerFooter: { paddingHorizontal: 24, paddingBottom: 32, paddingTop: 16 },
  connRow:      { flexDirection: 'row', alignItems: 'center', paddingHorizontal: 24, paddingVertical: 16, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: 'rgba(255,255,255,0.25)' },
  connInfo:     { flex: 1 },
  connLabel:    { color: '#fff', fontSize: 16, fontWeight: '600' },
  connHost:     { color: 'rgba(255,255,255,0.7)', fontSize: 12, marginTop: 2 },
  connDelete:   { color: 'rgba(255,255,255,0.6)', fontSize: 22, paddingLeft: 16 },

  // QR scanner
  creatureImg:       { width: 120, height: 120, borderRadius: 26, marginBottom: 12 },
  scannerFull:       { ...StyleSheet.absoluteFillObject, backgroundColor: '#000', zIndex: 100 },
  scannerOverlay:    { ...StyleSheet.absoluteFillObject, alignItems: 'center', justifyContent: 'space-between', paddingVertical: 60 },
  scannerTopBar:     { alignItems: 'center', gap: 8, paddingHorizontal: 32 },
  scannerTitle:      { color: '#fff', fontSize: 20, fontWeight: '700' },
  scannerSubtitle:   { color: 'rgba(255,255,255,0.6)', fontSize: 14, textAlign: 'center', lineHeight: 20 },
  scannerReticle:    { width: 240, height: 240 },
  scannerCorner:     { position: 'absolute', width: 28, height: 28, borderColor: C.accent, borderWidth: 3 },
  cornerTL:          { top: 0, left: 0, borderRightWidth: 0, borderBottomWidth: 0, borderTopLeftRadius: 4 },
  cornerTR:          { top: 0, right: 0, borderLeftWidth: 0, borderBottomWidth: 0, borderTopRightRadius: 4 },
  cornerBL:          { bottom: 0, left: 0, borderRightWidth: 0, borderTopWidth: 0, borderBottomLeftRadius: 4 },
  cornerBR:          { bottom: 0, right: 0, borderLeftWidth: 0, borderTopWidth: 0, borderBottomRightRadius: 4 },
  scannerCancel:     { backgroundColor: 'rgba(255,255,255,0.15)', borderRadius: 24, paddingVertical: 12, paddingHorizontal: 32 },
  scannerCancelText: { color: '#fff', fontSize: 16, fontWeight: '600' },
  scannerError:      { color: C.red, fontSize: 16, textAlign: 'center', marginBottom: 24 },

  // Chat layout
  safe:         { flex: 1, backgroundColor: C.bg },
  paneArea:     { flex: 1 },

  // Header
  header:       { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: 16, paddingVertical: 11, borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: C.border },
  headerLeft:   { flexDirection: 'row', alignItems: 'center', gap: 8 },
  backBtn:      { paddingRight: 4, paddingVertical: 2 },
  backBtnText:  { fontSize: 32, lineHeight: 34, color: C.accent, fontWeight: '300' },
  clearBtn:     { paddingVertical: 4, paddingHorizontal: 2 },
  clearBtnText: { fontSize: 14, color: C.textSecondary, fontWeight: '500' },
  headerTitle:  { fontSize: 17, fontWeight: '700', color: C.textPrimary, letterSpacing: 1 },
  headerRepo:   { fontSize: 11, color: C.textSecondary, marginTop: 1 },
  connDot:      { width: 8, height: 8, borderRadius: 4 },

  // Chat pane
  pane:              { flex: 1, backgroundColor: C.bg },
  messageList:       { flex: 1 },
  messageListContent: { paddingVertical: 16 },
  emptyState:        { textAlign: 'center', color: C.textMuted, fontSize: 14, marginTop: 80 },
  reconnectBanner:   { flexDirection: 'row', alignItems: 'center', justifyContent: 'center', paddingVertical: 7, backgroundColor: '#fffbeb', borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: '#fef3c7' },
  reconnectText:     { color: C.yellow, fontSize: 12, fontWeight: '500' },

  // Messages
  messageWrap:       { paddingHorizontal: 14, marginBottom: 14 },
  messageWrapRight:  { alignItems: 'flex-end' },
  messageLabel:      { fontSize: 11, color: C.textMuted, marginBottom: 4, marginLeft: 2, fontWeight: '600', letterSpacing: 0.5, textTransform: 'uppercase' },
  messageLabelRight: { marginLeft: 0, marginRight: 2 },
  textBlock:         { color: C.textPrimary, fontSize: 17, lineHeight: 26 },
  cursor:            { color: C.accent, fontSize: 14 },
  questionMark:      { color: C.yellow, fontWeight: '700', fontSize: 15, marginBottom: 2 },
  costLabel:         { fontSize: 11, color: C.textMuted, marginTop: 4, marginLeft: 2 },

  // Input bar
  inputRow:     { flexDirection: 'row', alignItems: 'flex-end', paddingHorizontal: 12, paddingVertical: 10, paddingBottom: Platform.OS === 'android' ? 14 : 10, gap: 8, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: C.border, backgroundColor: C.surface },
  input:        { flex: 1, backgroundColor: C.bg, borderWidth: 1, borderColor: C.inputBorder, borderRadius: 12, paddingHorizontal: 14, paddingVertical: 12, color: C.textPrimary, fontSize: 17, lineHeight: 24 },
  stopBtnText:  { fontSize: 14, color: C.red, fontWeight: '600' },
})

