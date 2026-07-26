// Cause graph (storage-backend E2E suite orchestration):
//   C1 suite=durable -> E1 share one temporary SQLite storage directory
//   C2 suite=fs      -> E2 share one temporary directory with AWAKEN_STORE=fs
//   C3 child fails   -> E3 stop immediately and preserve the child's exit status
//   C4 suite ends    -> E4 remove the temporary storage directory
//
// Decision table:
//   Rule  C1  C2  C3  Expected
//   T1    Y   N   N   E1 + E4
//   T2    N   Y   N   E2 + E4
//   T3    -   -   Y   E3 + E4
//
// This replaces shell-specific `sh -c`/`mktemp` orchestration so the exact
// shared-storage suites run unchanged on Windows, macOS, and Linux.

import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const suites = {
  durable: [
    'managed_e2e.mjs',
    'managed_hitl_e2e.mjs',
    'managed_mcp_e2e.ts',
    'managed_restart_e2e.mjs',
    'managed_durable_e2e.mjs',
  ],
  fs: [
    'managed_statemachine_e2e.mjs',
    'ai_sdk_e2e.mjs',
    'ag_ui_e2e.mjs',
    'a2a_e2e.mjs',
    'acp_e2e.mjs',
    'managed_mcp_e2e.ts',
    'secret_nonleak_e2e.mjs',
    'managed_restart_e2e.mjs',
  ],
};

const suite = process.argv[2];
if (!(suite in suites)) {
  console.error(`usage: node storage_backend_suite.mjs <${Object.keys(suites).join('|')}>`);
  process.exit(2);
}

const cwd = path.dirname(fileURLToPath(import.meta.url));
const storageDir = mkdtempSync(path.join(tmpdir(), `awaken-${suite}-suite-`));
const env = {
  ...process.env,
  AWAKEN_STORAGE_DIR: process.env.AWAKEN_STORAGE_DIR || storageDir,
};
if (suite === 'fs') env.AWAKEN_STORE = 'fs';

let exitCode = 0;
try {
  for (const test of suites[suite]) {
    console.log(`RUN ${suite}: ${test}`);
    const result = spawnSync(process.execPath, [test], { cwd, env, stdio: 'inherit' });
    if (result.error) throw result.error;
    if (result.status !== 0) {
      exitCode = result.status ?? 1;
      break;
    }
  }
} finally {
  rmSync(storageDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}

process.exitCode = exitCode;
