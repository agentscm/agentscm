import { defineConfig, devices } from '@playwright/test'

const uiPort = process.env['AGENTSCM_UI_PORT'] ?? '5184'
const desktop = { viewport: { width: 1360, height: 900 } }
const mobileSpec = /mobile\.spec\.ts/

export default defineConfig({
  testDir: 'tests/e2e',
  // One live hub with shared state: run serially so tests don't race each other's writes.
  workers: 1,
  fullyParallel: false,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: [['list']],
  use: { baseURL: `http://127.0.0.1:${uiPort}`, trace: 'retain-on-failure' },
  projects: [
    // Desktop: the whole suite in all three engines.
    { name: 'chromium', use: { ...devices['Desktop Chrome'], ...desktop }, testIgnore: mobileSpec },
    { name: 'firefox', use: { ...devices['Desktop Firefox'], ...desktop }, testIgnore: mobileSpec },
    { name: 'webkit', use: { ...devices['Desktop Safari'], ...desktop }, testIgnore: mobileSpec },
    // Phones: the drawer and narrow-header flows on real device viewports and touch.
    { name: 'mobile-chrome', use: { ...devices['Pixel 7'] }, testMatch: mobileSpec },
    { name: 'mobile-safari', use: { ...devices['iPhone 14'] }, testMatch: mobileSpec },
  ],
  webServer: { command: `npx vite --port ${uiPort} --strictPort`, url: `http://127.0.0.1:${uiPort}`, reuseExistingServer: true, timeout: 60_000 },
})
