const { defineConfig, devices } = require('@playwright/test');

module.exports = defineConfig({
  testDir: './tests/browser',
  globalSetup: require.resolve('./tests/browser/build.cjs'),
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: 0,
  workers: process.env.CI ? 2 : 4,
  timeout: 30_000,
  expect: { timeout: 10_000 },
  reporter: [['list'], ['html', { open: 'never' }], ['junit', { outputFile: 'test-results/browser.xml' }]],
  use: {
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure',
  },
  projects: [
    { name: 'chromium', use: { ...devices['Desktop Chrome'] } },
    { name: 'firefox', use: { ...devices['Desktop Firefox'] } },
    { name: 'webkit', use: { ...devices['Desktop Safari'] } },
    { name: 'mobile-chromium', use: { ...devices['Pixel 7'] } },
    { name: 'mobile-webkit', use: { ...devices['iPhone 13'] } },
  ],
});
