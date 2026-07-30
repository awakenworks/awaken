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
      // Worktrees can otherwise race on the repository-level Cargo target
      // directory and load stale trait metadata from another branch. Each run
      // also gets a fresh data root instead of inheriting ~/.awaken migrations.
      // Browser E2E owns no human who can consume the one-time setup handoff.
      // Exercise application behavior in the explicit no-login deployment mode;
      // local-browser authentication has its own control-plane integration tests.
      command: "e2e_data_dir=$(mktemp -d /tmp/awaken-console-e2e.XXXXXX) && CARGO_TARGET_DIR=/tmp/awaken-target-console-e2e exec cargo run --quiet -p awaken-cli --bin awaken -- start --port 38080 --data-dir \"$e2e_data_dir\" --no-browser --identity-mode no-login",
      cwd: "..",
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
