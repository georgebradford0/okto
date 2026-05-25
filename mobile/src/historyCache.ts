// Persistent on-device cache of chat history per agent. Keyed by a caller-
// supplied stable identity (e.g. `master:<lair-pk>`, `agent:<lair-pk>:<name>`)
// — never the local tunnel proxy URL, which gets a fresh ephemeral port on
// every foreground reconnect. Backed by MMKV (synchronous, mmap'd) so a
// ChatPane can hydrate inside its initial useState callback — no spinner,
// no flash of empty list when switching between agents. The HTTP /history
// fetch then reconciles in the background and any divergence is patched via
// the existing LCP merge in App.tsx.
//
// Every helper is best-effort and swallows errors: if the cache is missing
// or corrupt we behave exactly as before (network fetch + staggered fade).

import { createMMKV } from 'react-native-mmkv'

// Bump when the persisted shape changes incompatibly. The version is folded
// into the storage key so stale rows from a prior schema are silently
// shadowed (and eventually overwritten) rather than parsed.
const VERSION = 1

// Per-chat row cap. The FlatList copes with far more in memory, but the
// cache only needs enough to feel "instant" on remount — older rows come
// back via the /history reconcile.
const MAX_MESSAGES = 500

// Skip the write if the stringified payload exceeds this. Guards against a
// pathologically large tool_result blob stalling the JS thread on every
// streaming chunk.
const MAX_BYTES = 1_000_000

// Stored shape — a subset of App.tsx's Message. `prevRole` is recomputed
// from neighbours by withPrevRoles on load. `running` is transient: a tool
// still spinning at save time would look stuck after reload, so we drop it.
export interface CachedMessage {
  id:      string
  role:    string
  text:    string
  cost?:   number
  output?: string
}

const storage = createMMKV({ id: 'okto-history' })

const keyFor = (key: string) => `history:v${VERSION}:${key}`

export function loadHistory(key: string): CachedMessage[] | null {
  try {
    const raw = storage.getString(keyFor(key))
    if (!raw) return null
    const parsed = JSON.parse(raw)
    return Array.isArray(parsed) ? (parsed as CachedMessage[]) : null
  } catch {
    return null
  }
}

export function saveHistory(key: string, messages: CachedMessage[]): void {
  try {
    const trimmed = messages.length > MAX_MESSAGES
      ? messages.slice(messages.length - MAX_MESSAGES)
      : messages
    const cleaned: CachedMessage[] = trimmed.map(m => ({
      id:   m.id,
      role: m.role,
      text: m.text,
      ...(m.cost   != null ? { cost:   m.cost }   : {}),
      ...(m.output != null ? { output: m.output } : {}),
    }))
    const json = JSON.stringify(cleaned)
    if (json.length > MAX_BYTES) return
    storage.set(keyFor(key), json)
  } catch {
    /* best-effort */
  }
}

export function clearHistory(key: string): void {
  try { storage.remove(keyFor(key)) } catch {}
}

export function clearAllHistory(): void {
  try { storage.clearAll() } catch {}
}
