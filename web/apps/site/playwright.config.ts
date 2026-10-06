import { defineConfig, devices } from '@playwright/test'

const deployedUrl = process.env.PLAYWRIGHT_BASE_URL

export default defineConfig({
  testDir: './e2e',
  testMatch: '**/*.e2e.ts',
  fullyParallel: true,
  outputDir: './test-results/.playwright',
  use: {
    baseURL: deployedUrl || 'http://127.0.0.1:3100',
    trace: 'retain-on-failure',
    reducedMotion: 'reduce',
    ...(process.platform === 'darwin' ? { channel: 'chrome' } : {}),
  },
  projects: [
    { name: 'desktop', use: { ...devices['Desktop Chrome'], viewport: { width: 1440, height: 1000 } } },
    { name: 'mobile', use: { ...devices['iPhone 13'], defaultBrowserType: 'chromium', deviceScaleFactor: 1 } },
  ],
  ...(deployedUrl ? {} : { webServer: { command: 'bun run preview --host 127.0.0.1 --port 3100', url: 'http://127.0.0.1:3100', reuseExistingServer: !process.env.CI } }),
})
