import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  testMatch: /a2a-official-sdk\.interop\.ts/,
  timeout: 600_000,
  expect: { timeout: 30_000 },
  retries: 0,
  workers: 1,
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
