import { defineConfig, devices } from "@playwright/test";

// UI e2e: drives the real console (vite dev on :3002, proxying /v1 to a real
// awaken-server in management mode on :38080). Both are launched as
// webServers; set AWAKEN_HTTP_URL to point vite's proxy at an existing backend.
export default defineConfig({
  testDir: "./e2e",
  // real-llm.spec.ts self-skips unless a model key is present (submitted via the
  // credential API), so it is inert in the default CI run and executes only when a
  // GOOGLE_API_KEY/GEMINI_API_KEY is provided to the test.
  timeout: 45_000,
  expect: { timeout: 10_000 },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [["list"]],
  use: {
    baseURL: "http://127.0.0.1:3002",
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: [
    {
      // The management backend (advertises /v1/capabilities, config plane, sessions…).
      // `awaken` (awaken-cli) is the production binary that subsumes awaken-server: its
      // default Serve role mounts the full management + data plane over AWAKEN_HTTP_ADDR.
      command: "cargo run --quiet -p awaken-cli --bin awaken",
      cwd: "..",
      env: { AWAKEN_HTTP_ADDR: "127.0.0.1:38080" },
      url: "http://127.0.0.1:38080/v1/config/catalog",
      timeout: 240_000,
      reuseExistingServer: true,
    },
    {
      command: "pnpm dev",
      url: "http://127.0.0.1:3002",
      timeout: 60_000,
      reuseExistingServer: true,
    },
  ],
});
