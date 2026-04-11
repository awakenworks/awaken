import { defineConfig } from '@playwright/test';

const adminStorageDir =
  process.env.AWAKEN_E2E_ADMIN_STORAGE_DIR ??
  `./target/e2e-admin-sessions/${Date.now()}-${process.pid}`;

export default defineConfig({
  testDir: './tests',
  testMatch: /admin-config-ui\.spec\.ts/,
  timeout: 180_000,
  expect: { timeout: 30_000 },
  retries: 0,
  use: {
    baseURL: 'http://127.0.0.1:3002',
  },
  webServer: [
    {
      command: `AWAKEN_STORAGE_DIR=${adminStorageDir} cargo run -p ai-sdk-starter-agent`,
      cwd: '..',
      port: 38080,
      timeout: 120_000,
      reuseExistingServer: false,
    },
    {
      command: 'env -u NO_COLOR -u FORCE_COLOR npm --prefix apps/admin-console run dev',
      cwd: '..',
      port: 3002,
      timeout: 120_000,
      reuseExistingServer: false,
    },
  ],
});
