// QR-code payload parser. Matches the format lair prints in
// `bootstrap::print_qr`: `2:<host>:<port>:<pk_base32>`.
//
// Mobile reads this via a camera scanner; on desktop the user pastes the
// payload text (printed in the lair log alongside the QR). Same wire format.

export interface QrPayload {
  v:    2
  host: string
  port: number
  pk:   string  // base32(noise static pubkey)
}

export function parseQrPayload(raw: string): QrPayload | null {
  const parts = raw.trim().split(':')
  if (parts.length !== 4 || parts[0] !== '2') return null
  const [, host, portStr, pk] = parts
  const port = Number.parseInt(portStr, 10)
  if (!host || !pk || !Number.isFinite(port) || port <= 0 || port > 65535) return null
  return { v: 2, host, port, pk }
}

/** Inverse of `parseQrPayload`. Used to prefill the connect-screen textarea
 *  from a stored QR so a failed auto-reconnect leaves the user one click
 *  away from retrying instead of forcing a re-paste. */
export function formatQrPayload(p: QrPayload): string {
  return `${p.v}:${p.host}:${p.port}:${p.pk}`
}
