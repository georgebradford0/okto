/* eslint-disable no-undef */
// Runs after the test framework is set up. @testing-library/react-native v13
// auto-extends `expect` with its matchers and auto-cleans between tests, so the
// only thing left to do is hush the unavoidable noise the RN Animated driver and
// async state updates emit under jest — these are framework artefacts, not test
// failures, and drowning the real signal makes diffs unreadable.

const SILENCED = [
  'useNativeDriver',
  'not wrapped in act',
  'AnimatedComponent',
  'forwardRef render functions accept exactly two parameters',
  // App.tsx's own diagnostic logger (`logE`) routes through console.error with a
  // `[timestamp] ERROR` prefix — app behaviour, not a test failure.
  '] ERROR',
]

const realError = console.error
beforeAll(() => {
  jest.spyOn(console, 'error').mockImplementation((...args) => {
    const msg = typeof args[0] === 'string' ? args[0] : ''
    if (SILENCED.some(s => msg.includes(s))) return
    realError(...args)
  })
  // App.tsx logs prolifically via console.log; mute it so test output is signal.
  jest.spyOn(console, 'log').mockImplementation(() => {})
})

afterAll(() => {
  console.error.mockRestore?.()
  console.log.mockRestore?.()
})
