import { defineConfig } from '@playwright/test';

/**
 * Promo-demo recording config. Reuses the admin e2e webServer pattern but:
 *  - points the backend `default` provider path at real Vertex Gemini (env
 *    `VERTEX_API_KEY` / `VERTEX_PROJECT_ID` / `VERTEX_LOCATION` are forwarded),
 *  - exposes trace/dataset/eval routes,
 *  - records a continuous video per test (`video: 'on'`),
 *  - reuses already-running dev servers when present (so the same backend the
 *    spec was spot-checked against is the one recorded).
 *
 * Run:
 *   export VERTEX_API_KEY=$(gcloud auth print-access-token)
 *   export VERTEX_PROJECT_ID=uncarve-ai VERTEX_LOCATION=us-central1
 *   DEMO_LOCALE=en npm run record:demo
 *   DEMO_LOCALE=zh npm run record:demo
 */

const ADMIN_TOKEN = process.env.DEMO_ADMIN_TOKEN ?? 'demo-bearer-token';
const LOCALE_DIR =
  process.env.DEMO_LOCALE === 'zh' || process.env.DEMO_LOCALE === 'zh-CN'
    ? 'zh'
    : 'en';
const STORAGE_DIR =
  process.env.AWAKEN_STORAGE_DIR ?? './target/awaken-demo-rec';

const backendEnv = [
  `AWAKEN_HTTP_ADDR=127.0.0.1:38080`,
  `AWAKEN_ADMIN_API_BEARER_TOKEN=${ADMIN_TOKEN}`,
  `AWAKEN_STORAGE_DIR=${STORAGE_DIR}`,
  `AWAKEN_SEED_PROFILE=demo`,
  `AWAKEN_EXPOSE_TRACE_ROUTES=true`,
  process.env.VERTEX_API_KEY ? `VERTEX_API_KEY=${process.env.VERTEX_API_KEY}` : '',
  `VERTEX_PROJECT_ID=${process.env.VERTEX_PROJECT_ID ?? 'project-wp-mtj-201'}`,
  `VERTEX_LOCATION=${process.env.VERTEX_LOCATION ?? 'us-central1'}`,
]
  .filter(Boolean)
  .join(' ');

export default defineConfig({
  testDir: './tests',
  testMatch: '**/admin-demo.spec.ts',
  timeout: 1_200_000,
  expect: { timeout: 30_000 },
  retries: 0,
  workers: 1,
  outputDir: `./target/demo-recordings/${LOCALE_DIR}`,
  reporter: [['list']],
  use: {
    baseURL: 'http://127.0.0.1:3002',
    // Taller/wider so the dense agent-editor fits without scrolling the topbar
    // off-frame; deviceScaleFactor 2 renders retina-crisp text.
    viewport: { width: 1600, height: 1000 },
    deviceScaleFactor: 2,
    video: 'off',
    actionTimeout: 30_000,
    navigationTimeout: 60_000,
    // Headless Chromium + continuous video recording can exhaust the default
    // /dev/shm and crash the renderer ("Target crashed"). These args avoid it.
    launchOptions: {
      args: [
        '--disable-dev-shm-usage',
        '--disable-gpu',
        '--no-sandbox',
        '--disable-features=site-per-process',
      ],
    },
  },
  webServer: [
    {
      command: `${backendEnv} cargo run -p ai-sdk-starter-agent`,
      cwd: '..',
      port: 38080,
      timeout: 240_000,
      reuseExistingServer: true,
    },
    {
      command: 'env -u NO_COLOR -u FORCE_COLOR npm --prefix apps/admin-console run dev',
      cwd: '..',
      port: 3002,
      timeout: 120_000,
      reuseExistingServer: true,
    },
  ],
});
