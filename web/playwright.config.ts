import { defineConfig, devices } from "@playwright/test";

// UI e2e: drives the real console (vite dev on :3002, proxying /v1 to a real
// awaken-server-local in management mode on :38080). Both are launched as
// webServers; set AWAKEN_HTTP_URL to point vite's proxy at an existing backend.
export default defineConfig({
  testDir: "./e2e",
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
      command: "cargo run --quiet -p awaken-server-local",
      cwd: "..",
      env: { AWAKEN_MODEL_MODE: "management", AWAKEN_HTTP_ADDR: "127.0.0.1:38080" },
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
