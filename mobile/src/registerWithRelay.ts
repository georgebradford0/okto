// Tunnel-driven push-notification registration.
//
// On every successful chat-screen mount we hit the lair's /info endpoint over
// the encrypted Noise tunnel; lair returns its Ed25519 relay-signing pubkey
// alongside the URL of the relay it pushes through. We then ask iOS for the
// APNs device token and bind it to that pubkey via a two-step, ownership-
// proving handshake:
//   1. POST /register/challenge — the relay sends a silent push carrying a
//      nonce to the token.
//   2. The native layer receives that push and hands us the nonce.
//   3. POST /register with the nonce — proving we actually control the token,
//      not merely know its value.
// Idempotent — re-running on every reconnect just refreshes the relay row.
//
// All errors are logged and swallowed; missing push capability never breaks
// the rest of the app.

import { Platform } from 'react-native'
import NativePush from './NativePush'

interface LairInfo {
  pubkey:               string
  relay_signing_pubkey?: string
  relay_url?:           string
}

const registered = new Set<string>()

export async function registerWithRelay(baseUrl: string, log: (m: string) => void): Promise<void> {
  if (Platform.OS !== 'ios')         return  // FCM not wired yet
  if (registered.has(baseUrl))       return
  if (!NativePush)                   { log('[push] native module not registered'); return }

  let info: LairInfo
  try {
    const r = await fetch(`${baseUrl}/info`)
    if (!r.ok) { log(`[push] /info HTTP ${r.status}`); return }
    info = await r.json() as LairInfo
  } catch (e) {
    log(`[push] /info fetch failed: ${String(e)}`)
    return
  }

  if (!info.relay_signing_pubkey || !info.relay_url) {
    log('[push] lair did not advertise a relay — skipping')
    return
  }

  let token: string | null
  try {
    token = await NativePush.requestPermissionAndRegister()
  } catch (e) {
    log(`[push] APNs registration failed: ${String(e)}`)
    return
  }
  if (!token) {
    log('[push] notifications declined by user')
    return
  }

  // Which APNs gateway this build's token resolves on. The relay needs to
  // know because a token minted under a development provisioning profile only
  // works on the sandbox gateway, regardless of what okto.entitlements says.
  let environment: 'sandbox' | 'production' = 'production'
  try {
    environment = await NativePush.apsEnvironment()
  } catch (e) {
    log(`[push] apsEnvironment failed, defaulting to production: ${String(e)}`)
  }

  const relay = info.relay_url.replace(/\/$/, '')

  // Step 1 — ask the relay to prove we control this device token. It sends a
  // nonce only as a silent push to the token; the HTTP response carries none.
  try {
    const res = await fetch(`${relay}/register/challenge`, {
      method:  'POST',
      headers: { 'content-type': 'application/json' },
      body:    JSON.stringify({ device_token: token, platform: 'ios', environment }),
    })
    if (!res.ok) { log(`[push] /register/challenge HTTP ${res.status}`); return }
  } catch (e) {
    log(`[push] /register/challenge fetch failed: ${String(e)}`)
    return
  }

  // Step 2 — wait for that silent push and read the nonce out of it. Only the
  // device that genuinely owns the token receives it.
  let nonce: string | null
  try {
    nonce = await NativePush.awaitRegistrationChallenge(20000)
  } catch (e) {
    log(`[push] awaiting registration challenge failed: ${String(e)}`)
    return
  }
  if (!nonce) {
    log('[push] registration challenge timed out — will retry next mount')
    return
  }

  // Step 3 — complete registration, echoing the nonce as proof of ownership.
  try {
    const res = await fetch(`${relay}/register`, {
      method:  'POST',
      headers: { 'content-type': 'application/json' },
      body:    JSON.stringify({
        device_token:    token,
        platform:        'ios',
        lair_pubkey:     info.relay_signing_pubkey,
        challenge_nonce: nonce,
        environment,
      }),
    })
    if (!res.ok) {
      log(`[push] /register HTTP ${res.status}`)
      return
    }
    registered.add(baseUrl)
    log(`[push] registered with relay ${relay} for pubkey ${info.relay_signing_pubkey}`)
  } catch (e) {
    log(`[push] /register fetch failed: ${String(e)}`)
  }
}
