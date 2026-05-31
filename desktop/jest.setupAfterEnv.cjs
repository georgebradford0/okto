/* eslint-disable no-undef */
// Runs after the test framework is set up. Adds the jest-dom matchers and
// auto-cleanup, then hushes the unavoidable noise the app emits under jsdom —
// these are framework/app artefacts, not test failures, and drowning the real
// signal makes diffs unreadable.

require('@testing-library/jest-dom')
const { cleanup } = require('@testing-library/react')

afterEach(() => cleanup())

const SILENCED = [
  'not wrapped in act',
  // App.tsx's own diagnostic logger routes through console.error/log — app
  // behaviour, not a test failure.
  '] ERROR',
  'React does not recognize',
  'Received `true` for a non-boolean attribute',
]

beforeAll(() => {
  jest.spyOn(console, 'error').mockImplementation((...args) => {
    const msg = typeof args[0] === 'string' ? args[0] : ''
    if (SILENCED.some(s => msg.includes(s))) return
    // eslint-disable-next-line no-console
    console.warn(...args)
  })
  // App.tsx logs prolifically via console.log; mute it so test output is signal.
  jest.spyOn(console, 'log').mockImplementation(() => {})
})

afterAll(() => {
  console.error.mockRestore?.()
  console.log.mockRestore?.()
})
