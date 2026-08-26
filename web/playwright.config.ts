import { defineConfig, devices } from "@playwright/test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const backendPort = Number(process.env.AWAKEN_E2E_BACKEND_PORT ?? "38080");
const webPort = Number(process.env.AWAKEN_E2E_WEB_PORT ?? "3002");
const backendUrl = `http://127.0.0.1:${backendPort}`;
const webUrl = `http://127.0.0.1:${webPort}`;
const cargoTargetDir = process.env.AWAKEN_E2E_CARGO_TARGET_DIR ?? "/tmp/awaken-target-console-e2e";
const backendDataDir = process.env.AWAKEN_E2E_OWNED_DATA_DIR
  ?? mkdtempSync(join(tmpdir(), "awaken-console-e2e."));
process.env.AWAKEN_E2E_OWNED_DATA_DIR = backendDataDir;

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
  globalTeardown: "./e2e/global-teardown.ts",
  use: {
    baseURL: webUrl,
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
      command: `CARGO_TARGET_DIR="${cargoTargetDir}" cargo run --quiet -p awaken-cli --bin awaken -- all-in-one --config web/e2e/browser-e2e.toml --port ${backendPort} --data-dir "${backendDataDir}" --no-browser --identity-mode no-login`,
      cwd: "..",
      url: `${backendUrl}/v1/config/catalog`,
      // A cold Rust build on constrained CI runners can exceed four minutes;
      // keep the browser gate reliable while still bounding startup.
      timeout: 600_000,
      reuseExistingServer: true,
    },
    {
      command: `AWAKEN_HTTP_URL=${backendUrl} pnpm exec vite --host 127.0.0.1 --port ${webPort}`,
      url: webUrl,
      timeout: 60_000,
      reuseExistingServer: true,
    },
  ],
});
