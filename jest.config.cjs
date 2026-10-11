module.exports = {
  watchman: false,
  testEnvironment: 'jsdom',
  testEnvironmentOptions: { url: 'http://localhost/' },
  testMatch: ['<rootDir>/tests/web/**/*.test.cjs'],
  collectCoverageFrom: ['remote/web/**/*.js'],
  coverageDirectory: 'coverage/web',
  coverageReporters: ['text', 'html', 'lcov', 'json-summary', 'json'],
  coverageThreshold: {
    global: { statements: 99, branches: 94, functions: 100, lines: 100 },
  },
  clearMocks: true,
  restoreMocks: true,
};
