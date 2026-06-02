/* eslint-disable no-undef */
// Global test environment for the mobile app's behavioural tests.
//
// App.tsx is a single large component tree wired to a stack of native modules
// (Noise tunnel, camera, MMKV, reanimated, keyboard-controller, safe-area) and
// the two network surfaces (WebSocket + fetch). We mock the native layer with
// inert stand-ins and install controllable fakes for the network so a test can
// render <App/> and drive a full connect → stream → chat flow.

// @testing-library/react-native asserts react-test-renderer's version matches
// react's. Under the npm-workspace + git-worktree layout that probe misreads
// the hoisted react version; the renderer is genuinely compatible, so skip it.
process.env.RNTL_SKIP_DEPS_CHECK = '1'

const { FakeWebSocket, fakeFetch } = require('./__tests__/helpers/server')

// ── Network globals ───────────────────────────────────────────────────────────
global.WebSocket = FakeWebSocket
global.fetch = fakeFetch

// ── Shared design-system (Tamagui) → plain RN primitives ──────────────────────
jest.mock('@okto/ui', () => require('./__tests__/helpers/oktoUiMock'))

// ── lucide icons → inert ──────────────────────────────────────────────────────
jest.mock('lucide-react-native', () => ({ Send: () => null }))

// ── reanimated → plain components (the worklet runtime is not present) ─────────
jest.mock('react-native-reanimated', () => {
  const RN = require('react-native')
  return {
    __esModule: true,
    default: { View: RN.View, Text: RN.Text, ScrollView: RN.ScrollView },
    View: RN.View,
    useAnimatedStyle: () => ({}),
    useSharedValue: v => ({ value: v }),
    withTiming: v => v,
    withSpring: v => v,
    Easing: { linear: () => 0, inout: () => 0, ease: () => 0 },
  }
})

// ── react-native-svg → inert host stubs (shimmer overlay needs no real SVG) ───
jest.mock('react-native-svg', () => {
  const RN = require('react-native')
  const Stub = () => null
  return {
    __esModule: true,
    default: RN.View,
    Svg: RN.View,
    Defs: Stub,
    LinearGradient: Stub,
    Stop: Stub,
    Rect: Stub,
  }
})

// ── keyboard-controller → passthrough provider + zeroed animation ─────────────
jest.mock('react-native-keyboard-controller', () => ({
  KeyboardProvider: ({ children }) => children,
  useReanimatedKeyboardAnimation: () => ({
    height: { value: 0 },
    progress: { value: 0 },
  }),
}))

// ── safe-area → zero insets, passthrough provider ─────────────────────────────
jest.mock('react-native-safe-area-context', () => {
  const RN = require('react-native')
  return {
    SafeAreaProvider: ({ children }) => children,
    SafeAreaView: RN.View,
    useSafeAreaInsets: () => ({ top: 0, bottom: 0, left: 0, right: 0 }),
  }
})

// ── vision-camera → inert camera; capture the scan callback for tests ─────────
jest.mock('react-native-vision-camera', () => {
  const scanState = { onCodeScanned: null }
  // Exposed so a test can simulate a QR detection: fire the captured callback.
  global.__cameraScan = value => {
    scanState.onCodeScanned?.([{ value }])
  }
  return {
    Camera: () => null,
    useCameraDevice: () => ({ id: 'back-camera' }),
    useCodeScanner: cfg => {
      scanState.onCodeScanned = cfg.onCodeScanned
      return {}
    },
  }
})

// ── MMKV → in-memory map ──────────────────────────────────────────────────────
jest.mock('react-native-mmkv', () => {
  const store = new Map()
  return {
    createMMKV: () => ({
      getString: k => store.get(k),
      set: (k, v) => store.set(k, v),
      remove: k => store.delete(k),
      clearAll: () => store.clear(),
    }),
  }
})

// ── AsyncStorage → official jest mock ─────────────────────────────────────────
jest.mock(
  '@react-native-async-storage/async-storage',
  () => require('@react-native-async-storage/async-storage/jest/async-storage-mock'),
)

// ── Native Noise tunnel → resolves a fixed local proxy port ───────────────────
jest.mock('./src/NativeNoiseConnection', () => ({
  __esModule: true,
  default: {
    connect: jest.fn(() => Promise.resolve(45678)),
    disconnect: jest.fn(),
  },
}))

// ── Native push → absent (registerWithRelay short-circuits) ───────────────────
jest.mock('./src/NativePush', () => ({ __esModule: true, default: null }))
