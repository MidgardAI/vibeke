// Playwright for Electron (spec 16 §16.3): launches the built app (`bun run build` first; the
// `e2e` script does it). Tests skip themselves when there is no display.

import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './e2e',
  testMatch: '**/*.e2e.ts',
  timeout: 90_000,
  expect: { timeout: 15_000 },
  workers: 1,
  reporter: [['list']],
  // Playwright empties this on every run; screenshots go to test-results/ itself and persist.
  outputDir: './test-results/.playwright',
});
