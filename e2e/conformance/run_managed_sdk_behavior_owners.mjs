// Execute every distinct real-process scenario that owns an official Managed
// SDK operation. The operation-to-owner relation is derived from the canonical
// generated SDK ledger by managed_ts_sdk_method_manifest.mjs; this runner owns
// no second resource list, method list, or package-script copy.

import { spawn } from 'node:child_process';
import { resolve } from 'node:path';
import { MANAGED_TS_METHOD_MANIFEST } from './managed_ts_sdk_method_manifest.mjs';

const E2E = resolve(import.meta.dirname, '..');
const TSX_CLI = resolve(E2E, 'node_modules/tsx/dist/cli.mjs');
const owners = [...new Set(MANAGED_TS_METHOD_MANIFEST.map(({ owner }) => owner))].sort();
const OWNER_TIMEOUT_MS = Number(process.env.AWAKEN_MANAGED_OWNER_TIMEOUT_MS ?? 180_000);
if (!Number.isSafeInteger(OWNER_TIMEOUT_MS) || OWNER_TIMEOUT_MS <= 0) {
  throw new Error('AWAKEN_MANAGED_OWNER_TIMEOUT_MS must be a positive safe integer');
}

// Causal coverage graph:
// C1 the current official SDK generates one canonical HTTP operation ledger;
// C2 the ownership projection binds every operation/helper to one scenario;
// C3 this runner executes every distinct owner in a fresh child process.
// Effects: E1 no owner is skipped because a package script was hand-edited;
// E2 one owner shared by many methods executes once; E3 a signal, spawn error,
// non-zero result, or capability skip fails the compatibility gate immediately.
// Decision table: C1+C2+C3 => E1+E2; failed/skipped child => E3. Static source
// invocation and exact SDK-surface checks run immediately before this gate and
// prove that an owner actually calls each method attributed to it.
for (const [index, owner] of owners.entries()) {
  console.log(`[managed-sdk ${index + 1}/${owners.length}] ${owner}`);
  const args = owner.endsWith('.ts') ? [TSX_CLI, owner] : [owner];
  await new Promise((resolveOwner, rejectOwner) => {
    const child = spawn(process.execPath, args, {
      cwd: E2E,
      env: process.env,
      stdio: ['inherit', 'pipe', 'pipe'],
    });
    let output = '';
    let timedOut = false;
    const timeout = setTimeout(() => {
      timedOut = true;
      child.kill('SIGKILL');
    }, OWNER_TIMEOUT_MS);
    const tee = (chunk, destination) => {
      const text = chunk.toString();
      output += text;
      destination.write(text);
    };
    child.stdout.on('data', (chunk) => tee(chunk, process.stdout));
    child.stderr.on('data', (chunk) => tee(chunk, process.stderr));
    child.once('error', (error) => {
      clearTimeout(timeout);
      rejectOwner(error);
    });
    child.once('close', (status, signal) => {
      clearTimeout(timeout);
      if (timedOut) {
        rejectOwner(new Error(`${owner}: exceeded ${OWNER_TIMEOUT_MS}ms timeout`));
      } else if (signal) {
        rejectOwner(new Error(`${owner}: terminated by ${signal}`));
      } else if (status !== 0) {
        rejectOwner(new Error(`${owner}: exited with status ${status}`));
      } else if (/^\s*(?:E2E )?SKIP:/mu.test(output)) {
        rejectOwner(new Error(`${owner}: skipped and therefore supplied no compatibility evidence`));
      } else {
        resolveOwner();
      }
    });
  });
}

console.log(
  `Managed SDK behavior owners PASS: ${MANAGED_TS_METHOD_MANIFEST.length} methods/helpers -> ${owners.length} real-process scenarios.`,
);
