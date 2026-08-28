// Execute every distinct real-process scenario that owns an official Managed
// SDK operation. The operation-to-owner relation is derived from the canonical
// generated SDK ledger by managed_ts_sdk_method_manifest.mjs; this runner owns
// no second resource list, method list, or package-script copy.

import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import { MANAGED_TS_METHOD_MANIFEST } from './managed_ts_sdk_method_manifest.mjs';
import { assertOwnerOperationReceipts } from './managed_sdk_operation_receipts.mjs';

const E2E = resolve(import.meta.dirname, '..');
const TSX_CLI = resolve(E2E, 'node_modules/tsx/dist/cli.mjs');
const RECEIPT_HOOK = resolve(import.meta.dirname, 'managed_sdk_receipt_hook.mjs');
const RECEIPT_NODE_OPTIONS = [process.env.NODE_OPTIONS, `--import=${RECEIPT_HOOK}`]
  .filter(Boolean)
  .join(' ');
const owners = [...new Set(MANAGED_TS_METHOD_MANIFEST.map(({ owner }) => owner))].sort();
const OWNER_TIMEOUT_MS = Number(process.env.AWAKEN_MANAGED_OWNER_TIMEOUT_MS ?? 180_000);
if (!Number.isSafeInteger(OWNER_TIMEOUT_MS) || OWNER_TIMEOUT_MS <= 0) {
  throw new Error('AWAKEN_MANAGED_OWNER_TIMEOUT_MS must be a positive safe integer');
}

async function executeOwner(owner, receiptFile) {
  const args = owner.endsWith('.ts') ? [TSX_CLI, owner] : [owner];
  await new Promise((resolveOwner, rejectOwner) => {
    const child = spawn(process.execPath, args, {
      cwd: E2E,
      env: {
        ...process.env,
        AWAKEN_MANAGED_SDK_RECEIPT_FILE: receiptFile,
        NODE_OPTIONS: RECEIPT_NODE_OPTIONS,
      },
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

function loadReceipts(receiptFile) {
  return (existsSync(receiptFile) ? readFileSync(receiptFile, 'utf8') : '')
    .split('\n')
    .filter(Boolean)
    .map((line) => JSON.parse(line));
}

// Causal coverage graph:
// C1 the current official SDK generates one canonical HTTP operation ledger;
// C2 the ownership projection binds every operation/helper to one scenario;
// C3 this runner executes every distinct owner in a fresh child process.
// C4 an official-SDK-only transport hook records every runtime HTTP edge.
// Effects: E1 no owner is skipped because a package script was hand-edited;
// E2 one owner shared by many methods executes once; E3 a signal, spawn error,
// non-zero result, capability skip, or missing operation receipt fails the gate.
// Decision table: C1+C2+C3+C4 => E1+E2; failed/skipped child => E3. Static source
// invocation and exact SDK-surface checks run immediately before this gate;
// the receipts prove those attributed calls execute rather than merely exist.
const receiptDirectory = mkdtempSync(resolve(tmpdir(), 'awaken-managed-sdk-receipts-'));
let receiptCount = 0;
try {
  for (const [index, owner] of owners.entries()) {
    console.log(`[managed-sdk ${index + 1}/${owners.length}] ${owner}`);
    const receiptFile = resolve(receiptDirectory, `${index}.jsonl`);
    await executeOwner(owner, receiptFile);
    receiptCount += assertOwnerOperationReceipts(
      MANAGED_TS_METHOD_MANIFEST,
      owner,
      loadReceipts(receiptFile),
    );
  }
} finally {
  rmSync(receiptDirectory, { recursive: true, force: true });
}

console.log(
  `Managed SDK behavior owners PASS: ${receiptCount} HTTP operations + `
    + `${MANAGED_TS_METHOD_MANIFEST.length - receiptCount} helpers -> `
    + `${owners.length} real-process scenarios.`,
);
