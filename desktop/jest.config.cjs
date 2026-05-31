// Behavioural test runner for the desktop (Vite + react-native-web) renderer.
// Mirrors mobile/jest.config.js but runs under jsdom: App.tsx is a browser
// component (raw <textarea>, `localStorage`, browser `WebSocket`/`fetch`) whose
// only non-DOM surfaces — the Tauri bridge, @okto/ui, lucide — are mocked in
// jest.setup.cjs. So nothing under node_modules needs transforming.

// React is nested per-workspace in this monorepo (the root pins a single React
// for react-native), while the test tooling — `@testing-library/react` — is
// hoisted to the repo root. A hoisted package can't resolve `react` by walking
// up from root, so pin every `react` / `react-dom` import to the one canonical
// copy this workspace actually uses. Mirrors mobile/jest.config.js.
const path = require('path')
const reactDir = path.dirname(require.resolve('react/package.json'))
const reactDomDir = path.dirname(require.resolve('react-dom/package.json'))

module.exports = {
  testEnvironment: 'jsdom',
  rootDir: '.',
  moduleNameMapper: {
    '^react$': path.join(reactDir, 'index.js'),
    '^react/(.*)$': path.join(reactDir, '$1'),
    '^react-dom$': path.join(reactDomDir, 'index.js'),
    '^react-dom/(.*)$': path.join(reactDomDir, '$1'),
  },
  // Browser globals (WebSocket, fetch) + native-module mocks.
  setupFiles: ['<rootDir>/jest.setup.cjs'],
  // jest-dom matchers + console hush + RTL auto-cleanup.
  setupFilesAfterEnv: ['<rootDir>/jest.setupAfterEnv.cjs'],
  testMatch: ['<rootDir>/__tests__/**/*.test.tsx', '<rootDir>/__tests__/**/*.test.ts'],
  transform: {
    '^.+\\.(t|j)sx?$': [
      'babel-jest',
      {
        presets: [
          ['@babel/preset-env', { targets: { node: 'current' } }],
          ['@babel/preset-react', { runtime: 'automatic' }],
          '@babel/preset-typescript',
        ],
      },
    ],
  },
  transformIgnorePatterns: ['/node_modules/'],
}
