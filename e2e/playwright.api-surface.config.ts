import { defineConfig } from '@playwright/test';

export const TEST_ADMIN_TOKEN = 'test-bearer-api-surface';

export default defineConfig({
  testDir: './tests',
  testMatch: /api-surface\.spec\.ts/,
  timeout: 120_000,
  expect: { timeout: 30_000 },
  retries: 0,
  workers: 1,
  use: {
    baseURL: 'http://127.0.0.1:38080',
  },
  webServer: {
    command: `AWAKEN_SEED_PROFILE=demo AWAKEN_ADMIN_API_BEARER_TOKEN=${TEST_ADMIN_TOKEN} cargo run -p ai-sdk-starter-agent`,
    cwd: '..',
    port: 38080,
    timeout: 120_000,
    reuseExistingServer: false,
  },
});
