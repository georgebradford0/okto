module.exports = {
  preset: 'react-native',
  // Global mocks for native modules + browser globals (WebSocket, fetch).
  setupFiles: ['<rootDir>/jest.setup.js'],
  // Per-test cleanup + the @testing-library/react-native matchers.
  setupFilesAfterEnv: ['<rootDir>/jest.setupAfterEnv.js'],
  testMatch: ['<rootDir>/__tests__/**/*.test.tsx', '<rootDir>/__tests__/**/*.test.ts'],
  // `react` is nested in mobile/node_modules (workspace override) while the test
  // tooling is hoisted to the repo root, so root packages can't resolve `react`
  // by walking up. Pin every `react` import to the single canonical copy.
  moduleNameMapper: {
    '^react$': '<rootDir>/node_modules/react',
    '^react/(.*)$': '<rootDir>/node_modules/react/$1',
  },
  // Everything App.tsx pulls in from native land is mocked in jest.setup.js, so
  // the default RN transformIgnorePatterns is sufficient — the mocked modules are
  // never required for real.
};
