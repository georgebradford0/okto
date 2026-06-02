/**
 * Guard: the mobile wire-protocol constant must stay in lockstep with the
 * Rust source of truth (`okto_core::WIRE_PROTOCOL`). If someone bumps the
 * protocol on one side only, this fails the mobile gate (mobile-test.yml,
 * which the android/ios releases depend on) rather than shipping a client
 * that mis-reports the protocol it speaks. See PROTOCOL.md / CLAUDE.md.
 */
import fs from 'fs'
import path from 'path'
import { WIRE_PROTOCOL } from '../src/wire'

test('mobile WIRE_PROTOCOL matches okto_core::WIRE_PROTOCOL', () => {
  const libRs = fs.readFileSync(path.resolve(__dirname, '../../core/src/lib.rs'), 'utf8')
  const m = libRs.match(/pub const WIRE_PROTOCOL:\s*u32\s*=\s*(\d+)\s*;/)
  expect(m).not.toBeNull()
  expect(WIRE_PROTOCOL).toBe(Number(m![1]))
})
