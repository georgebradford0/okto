import { useEffect, useRef, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { parseQrPayload, type QrPayload } from './qr'
import { encodeClientFrame, parseServerEvent, type ServerEvent } from './wire'
import './App.css'

type Status =
  | { kind: 'idle' }
  | { kind: 'connecting'; target: QrPayload }
  | { kind: 'connected';  target: QrPayload; tunnelPort: number; ws: WebSocket }
  | { kind: 'error';      message: string }

function App() {
  const [qrInput, setQrInput] = useState('')
  const [status, setStatus]   = useState<Status>({ kind: 'idle' })
  const [events, setEvents]   = useState<ServerEvent[]>([])
  const [draft, setDraft]     = useState('')
  const logRef                = useRef<HTMLDivElement>(null)

  // Autoscroll the event log as new events arrive.
  useEffect(() => {
    const el = logRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [events])

  const connect = async () => {
    const target = parseQrPayload(qrInput)
    if (!target) {
      setStatus({ kind: 'error', message: 'invalid QR payload — expected 2:<host>:<port>:<pubkey>' })
      return
    }
    setStatus({ kind: 'connecting', target })
    setEvents([])
    try {
      const tunnelPort = await invoke<number>('noise_connect', {
        host:            target.host,
        port:            target.port,
        serverPubkeyB32: target.pk,
      })
      const ws = new WebSocket(`ws://127.0.0.1:${tunnelPort}/stream`)
      ws.onopen    = () => setStatus({ kind: 'connected', target, tunnelPort, ws })
      ws.onclose   = () => setStatus({ kind: 'idle' })
      ws.onerror   = () => setStatus({ kind: 'error', message: 'websocket error' })
      ws.onmessage = (e) => {
        const data = typeof e.data === 'string' ? e.data : ''
        const ev   = parseServerEvent(data)
        if (!ev) return
        // Auto-pong so lair's keepalive doesn't evict us. Don't display pings.
        if (ev.type === 'ping') {
          ws.send(encodeClientFrame({ type: 'pong', id: ev.id }))
          return
        }
        setEvents(prev => [...prev, ev])
      }
    } catch (e) {
      setStatus({ kind: 'error', message: String(e) })
    }
  }

  const send = () => {
    if (status.kind !== 'connected' || !draft.trim()) return
    status.ws.send(encodeClientFrame({ type: 'user_message', text: draft }))
    setDraft('')
  }

  const interrupt = () => {
    if (status.kind !== 'connected') return
    status.ws.send(encodeClientFrame({ type: 'interrupt' }))
  }

  const disconnect = () => {
    if (status.kind === 'connected') status.ws.close()
    else setStatus({ kind: 'idle' })
  }

  if (status.kind === 'connected') {
    return (
      <main className="container">
        <header style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline', gap: 12 }}>
          <h2 style={{ margin: 0 }}>Okto · {status.target.host}:{status.target.port}</h2>
          <span style={{ opacity: 0.6, fontSize: 12 }}>tunnel :{status.tunnelPort}</span>
        </header>

        <div
          ref={logRef}
          style={{
            margin: '16px 0',
            padding: 12,
            height: 360,
            overflowY: 'auto',
            background: '#0f0f0f',
            color: '#eaeaea',
            fontFamily: 'ui-monospace, SFMono-Regular, Menlo, monospace',
            fontSize: 12,
            textAlign: 'left',
            borderRadius: 6,
          }}
        >
          {events.length === 0 && <div style={{ opacity: 0.5 }}>waiting for events…</div>}
          {events.map((ev, i) => <EventLine key={i} ev={ev} />)}
        </div>

        <form
          onSubmit={(e) => { e.preventDefault(); send() }}
          style={{ display: 'flex', gap: 8 }}
        >
          <input
            value={draft}
            onChange={(e) => setDraft(e.currentTarget.value)}
            placeholder="message…"
            style={{ flex: 1 }}
          />
          <button type="submit" disabled={!draft.trim()}>Send</button>
          <button type="button" onClick={interrupt}>Interrupt</button>
          <button type="button" onClick={disconnect}>Disconnect</button>
        </form>
      </main>
    )
  }

  return (
    <main className="container">
      <h1>Okto</h1>
      <p style={{ opacity: 0.7 }}>
        Paste the QR payload printed by <code>lair</code> on startup. Format:{' '}
        <code>2:&lt;host&gt;:&lt;port&gt;:&lt;pubkey&gt;</code>
      </p>
      <textarea
        value={qrInput}
        onChange={(e) => setQrInput(e.currentTarget.value)}
        placeholder="2:1.2.3.4:9000:ABCDEF…"
        rows={3}
        style={{ width: '100%', fontFamily: 'monospace' }}
      />
      <div style={{ marginTop: 12 }}>
        <button onClick={connect} disabled={status.kind === 'connecting' || !qrInput.trim()}>
          {status.kind === 'connecting' ? 'Connecting…' : 'Connect'}
        </button>
      </div>
      {status.kind === 'error' && (
        <p style={{ color: '#c33', marginTop: 12 }}>{status.message}</p>
      )}
    </main>
  )
}

function EventLine({ ev }: { ev: ServerEvent }) {
  // Compact, per-type one-liner. Full payload is JSON-stringified for anything
  // we don't have a dedicated shape for.
  switch (ev.type) {
    case 'ready':       return <div style={{ color: '#9cf' }}>● ready  resumed={String(ev.resumed)}</div>
    case 'text':        return <span style={{ whiteSpace: 'pre-wrap' }}>{ev.text}</span>
    case 'tool_use':    return <div style={{ color: '#fc9' }}>▸ {ev.display ?? ev.tool}</div>
    case 'tool_output': return <div style={{ opacity: 0.7 }}>  {ev.line}</div>
    case 'tool_result': return <div style={{ color: '#9c9', opacity: 0.7 }}>  ✓ {String(ev.output).slice(0, 120)}</div>
    case 'done':        return <div style={{ color: '#9c9' }}>● done  ${ev.cost_usd.toFixed(4)}</div>
    case 'interrupted': return <div style={{ color: '#fc9' }}>● interrupted  ${ev.cost_usd.toFixed(4)}</div>
    case 'error':       return <div style={{ color: '#f99' }}>● error  {ev.message}</div>
    default:            return <div style={{ opacity: 0.6 }}>· {JSON.stringify(ev)}</div>
  }
}

export default App
