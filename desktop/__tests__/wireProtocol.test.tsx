/**
 * Guard: the desktop wire-protocol constant must stay in lockstep with the
 * Rust source of truth (`okto_core::WIRE_PROTOCOL`). Fails the desktop gate if
 * the protocol is bumped on one side only. See PROTOCOL.md / CLAUDE.md.
 */
import fs from 'fs'
import path from 'path'
import { WIRE_PROTOCOL } from '../src/wire'

test('desktop WIRE_PROTOCOL matches okto_core::WIRE_PROTOCOL', () => {
  const libRs = fs.readFileSync(path.resolve(__dirname, '../../core/src/lib.rs'), 'utf8')
  const m = libRs.match(/pub const WIRE_PROTOCOL:\s*u32\s*=\s*(\d+)\s*;/)
  expect(m).not.toBeNull()
  expect(WIRE_PROTOCOL).toBe(Number(m![1]))
})
