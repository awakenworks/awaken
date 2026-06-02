import { defineConfig } from '@playwright/test';

const TEST_ADMIN_TOKEN = 'test-bearer-interop-e2e';
const storageDir = `./target/e2e-interop-sessions/${Date.now()}-${process.pid}`;

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
    command: `AWAKEN_STORAGE_DIR=${storageDir} AWAKEN_SEED_PROFILE=demo AWAKEN_ADMIN_API_BEARER_TOKEN=${TEST_ADMIN_TOKEN} cargo run -p ai-sdk-starter-agent`,
    cwd: '..',
    port: 38080,
    timeout: 120_000,
    reuseExistingServer: false,
  },
});
