import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  timeout: 120_000,
  expect: { timeout: 30_000 },
  retries: 0,
  use: {
    baseURL: 'http://127.0.0.1:38080',
  },
  webServer: {
    command: 'cargo run -p ai-sdk-starter-agent',
    cwd: '..',
    port: 38080,
    timeout: 120_000,
    reuseExistingServer: false,
  },
});
