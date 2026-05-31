// Test doubles for the two network surfaces App.tsx talks to: the persistent
// `/stream` WebSocket and the `fetch`-based HTTP endpoints (/history,
// /completions, /clear, /agents/.../worktrees, …).
//
// Both are installed as globals by jest.setup.js. Tests drive the server side
// imperatively: grab the latest socket with `lastWs()`, push frames with
// `ws.mockServerEvent({...})`, and inspect what the client sent via `ws.sent`.
// HTTP responses are configured per-test with `onFetch(matcher, handler)`.

// ── Fake WebSocket ──────────────────────────────────────────────────────────

type WsListener = (ev: any) => void

export class FakeWebSocket {
  static CONNECTING = 0
  static OPEN = 1
  static CLOSING = 2
  static CLOSED = 3

  static instances: FakeWebSocket[] = []

  url: string
  readyState = FakeWebSocket.CONNECTING
  onopen: WsListener | null = null
  onmessage: WsListener | null = null
  onerror: WsListener | null = null
  onclose: WsListener | null = null
  /** Raw strings the client has sent (encoded ClientFrames). */
  sent: string[] = []

  constructor(url: string) {
    this.url = url
    FakeWebSocket.instances.push(this)
  }

  send(data: string) {
    this.sent.push(data)
  }

  close() {
    if (this.readyState === FakeWebSocket.CLOSED) return
    this.readyState = FakeWebSocket.CLOSED
    this.onclose?.({ code: 1000, reason: 'client close' })
  }

  // ── Test-only drivers ──────────────────────────────────────────────────────

  /** Simulate the TCP/WS handshake completing. */
  mockOpen() {
    this.readyState = FakeWebSocket.OPEN
    this.onopen?.({})
  }

  /** Push a typed ServerEvent to the client. */
  mockServerEvent(obj: unknown) {
    this.onmessage?.({ data: JSON.stringify(obj) })
  }

  /** Push a raw (possibly malformed) payload to the client. */
  mockServerRaw(raw: string) {
    this.onmessage?.({ data: raw })
  }

  /** Simulate an unexpected drop (triggers the client's reconnect backoff). */
  mockDrop(code = 1006) {
    this.readyState = FakeWebSocket.CLOSED
    this.onclose?.({ code, reason: '' })
  }

  /** The decoded ClientFrames the client has sent so far. */
  frames(): any[] {
    return this.sent.map(s => {
      try {
        return JSON.parse(s)
      } catch {
        return s
      }
    })
  }
}

/** The most recently constructed socket (the one the live ChatPane is using). */
export const lastWs = (): FakeWebSocket =>
  FakeWebSocket.instances[FakeWebSocket.instances.length - 1]

// ── Fake fetch ──────────────────────────────────────────────────────────────

type FetchHandler = (url: string, init?: any) => any

interface Route {
  match: (url: string, init?: any) => boolean
  handler: FetchHandler
}

const routes: Route[] = []

/** Recorded fetch calls — `{ url, init }` per call, in order. */
export const fetchCalls: Array<{ url: string; init?: any }> = []

/**
 * Register a fetch route. `matcher` is either a substring of the URL or a
 * predicate. Most-recently-registered routes win, so a test override added in
 * `beforeEach` shadows the defaults. The handler returns a body (status 200) or
 * a `reply(body, status)` spec.
 */
export function onFetch(
  matcher: string | ((url: string, init?: any) => boolean),
  handler: FetchHandler,
) {
  const match =
    typeof matcher === 'function' ? matcher : (url: string) => url.includes(matcher)
  routes.unshift({ match, handler })
}

/** Build a non-200 response from a handler. */
export const reply = (body: unknown, status = 200) =>
  ({ __fakeResponse: true as const, body, status })

const DEFAULTS: Route[] = [
  { match: u => u.includes('/history'), handler: () => ({ messages: [] }) },
  { match: u => u.includes('/completions'), handler: () => [] },
  { match: u => u.includes('/worktrees'), handler: () => [] },
  { match: u => u.includes('/clear'), handler: () => ({}) },
  { match: u => u.includes('/info'), handler: () => ({ pubkey: 'pk' }) },
]

function toResponse(v: any) {
  const spec = v && v.__fakeResponse ? v : { body: v, status: 200 }
  return {
    ok: spec.status >= 200 && spec.status < 300,
    status: spec.status,
    json: async () => spec.body,
    text: async () =>
      typeof spec.body === 'string' ? spec.body : JSON.stringify(spec.body),
  }
}

export function fakeFetch(url: string, init?: any): Promise<any> {
  fetchCalls.push({ url, init })
  const route =
    routes.find(r => r.match(url, init)) ?? DEFAULTS.find(r => r.match(url, init))
  const handler: FetchHandler = route ? route.handler : () => ({})
  return new Promise((resolve, reject) => {
    Promise.resolve(handler(url, init)).then(v => {
      if (init?.signal?.aborted) {
        const e = new Error('The operation was aborted')
        e.name = 'AbortError'
        reject(e)
        return
      }
      resolve(toResponse(v))
    }, reject)
  })
}

// ── Reset between tests ───────────────────────────────────────────────────────

/** Clear all sockets, routes and recorded calls. Call in `beforeEach`. */
export function resetServer() {
  FakeWebSocket.instances.length = 0
  routes.length = 0
  fetchCalls.length = 0
}
