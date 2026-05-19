import { defineConfig } from '@playwright/test';

/* Visual regression baseline.
 *
 * Runs against a live starter backend (port 38080) + admin-console dev
 * server (port 3002) + www dev server (port 3003). Each snapshot is
 * stored next to the spec under `visual.spec.ts-snapshots/`. CI compares
 * pixel-for-pixel against the committed baseline.
 *
 * To regenerate baselines (e.g. after a deliberate visual change):
 *   pnpm --filter awaken-e2e test:visual -- --update-snapshots
 *
 * Default `cwd` for webServer is `..` (repo root). */

const adminStorageDir =
  process.env.AWAKEN_E2E_VISUAL_STORAGE_DIR ??
  `./target/e2e-visual-sessions/${Date.now()}-${process.pid}`;

export const TEST_ADMIN_TOKEN = 'test-bearer-visual';

export default defineConfig({
  testDir: './tests',
  testMatch: '**/visual.spec.ts',
  timeout: 180_000,
  expect: {
    timeout: 30_000,
    /* Small per-pixel tolerance to absorb subpixel anti-aliasing differences
     * between local generation and CI rendering. Below 2% of pixels can
     * differ before a test is flagged. */
    toHaveScreenshot: {
      maxDiffPixelRatio: 0.02,
      animations: 'disabled',
    },
  },
  retries: 0,
  workers: 1,
  use: {
    viewport: { width: 1440, height: 900 },
  },
  webServer: [
    {
      command: `AWAKEN_STORAGE_DIR=${adminStorageDir} AWAKEN_ADMIN_API_BEARER_TOKEN=${TEST_ADMIN_TOKEN} cargo run -p ai-sdk-starter-agent`,
      cwd: '..',
      port: 38080,
      /* CI runners have a cold Cargo cache on first hit + heavy parallel
       * job load — `cargo run -p ai-sdk-starter-agent` can take 150-180s
       * to link. 120s timed out repeatedly; bump to 360s. */
      timeout: 360_000,
      reuseExistingServer: false,
    },
    {
      /* admin dev script already binds 127.0.0.1:3002 — no extra args needed. */
      command: 'env -u NO_COLOR -u FORCE_COLOR npm --prefix apps/admin-console run dev',
      cwd: '..',
      port: 3002,
      timeout: 180_000,
      reuseExistingServer: false,
    },
    {
      /* www dev script binds 127.0.0.1:3003. */
      command: 'npm --prefix apps/www run dev',
      cwd: '..',
      port: 3003,
      timeout: 180_000,
      reuseExistingServer: false,
    },
  ],
});
