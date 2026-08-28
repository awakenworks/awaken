import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import test from 'node:test';
import {
  assertOwnerOperationReceipts,
  receiptMatchesOperation,
} from './managed_sdk_operation_receipts.mjs';

const betaOperation = {
  sdkMethod: 'beta.files.retrieveMetadata',
  owner: 'files.mjs',
  method: 'GET',
  route: '/v1/files/{}',
  transportQuery: 'beta=true',
  betas: ['files-api-2025-04-14'],
};
const gaOperation = {
  sdkMethod: 'files.retrieveMetadata',
  owner: 'files.mjs',
  method: 'GET',
  route: '/v1/files/{}',
  betas: [],
};
const betaReceipt = {
  method: 'GET',
  path: '/v1/files/file_1',
  beta: 'true',
  betas: ['files-api-2025-04-14'],
  sdk: true,
  status: 200,
};

test('runtime receipts distinguish Beta and GA calls sharing one route', () => {
  // Cause/effect graph: C1=method/path are equal; C2=query selector differs;
  // C3=capability header differs. Effects: E1 the Beta receipt owns only the
  // Beta operation; E2 omitting either discriminator cannot certify GA as Beta
  // or Beta as GA. Decision table: C1+C2+C3=>E1; C1+(!C2||!C3)=>E2.
  assert.equal(receiptMatchesOperation(betaReceipt, betaOperation), true, 'C1+C2+C3/E1');
  assert.equal(receiptMatchesOperation(betaReceipt, gaOperation), false, 'C1+C2/E2');
  assert.equal(receiptMatchesOperation(
    { ...betaReceipt, beta: null },
    betaOperation,
  ), false, 'C1+!C2/E2');
  assert.equal(receiptMatchesOperation(
    { ...betaReceipt, betas: [] },
    betaOperation,
  ), false, 'C1+!C3/E2');
  assert.equal(receiptMatchesOperation(
    { ...betaReceipt, betas: [...betaReceipt.betas, 'managed-agents-2026-04-01'] },
    betaOperation,
  ), true, 'orthogonal capabilities compose without changing operation ownership');
});

test('only an official SDK request can satisfy executable ownership', () => {
  // Cause: a direct fetch can duplicate the public method, route and headers.
  // Effect: the Stainless SDK marker remains required, so source text plus a
  // hand-written fetch cannot masquerade as an official SDK behavior proof.
  assert.equal(receiptMatchesOperation({ ...betaReceipt, sdk: false }, betaOperation), false);
});

test('a request attempt without one non-server-fault response is not behavior evidence', () => {
  // Cause/effect partitions: no response, a 2xx/4xx application response, and
  // a 5xx server fault. Only a completed non-5xx exchange proves the SDK crossed
  // the product boundary; an attempted or failed transport cannot certify it.
  assert.equal(receiptMatchesOperation({ ...betaReceipt, status: undefined }, betaOperation), false);
  assert.equal(receiptMatchesOperation({ ...betaReceipt, status: 404 }, betaOperation), true);
  assert.equal(receiptMatchesOperation({ ...betaReceipt, status: 500 }, betaOperation), false);
});

test('owner qualification fails closed for every missing runtime edge', () => {
  // Decision table: R1 every owned operation has a matching runtime receipt ->
  // count all operations; R2 one receipt is absent or belongs to another owner
  // -> fail with the exact uncovered method. Extra support traffic is allowed
  // because lifecycle setup may call operations owned by another scenario.
  const manifest = [betaOperation, gaOperation];
  const gaReceipt = { ...betaReceipt, beta: null, betas: [] };
  assert.equal(
    assertOwnerOperationReceipts(manifest, 'files.mjs', [betaReceipt, gaReceipt]),
    2,
    'R1',
  );
  assert.throws(
    () => assertOwnerOperationReceipts(manifest, 'files.mjs', [betaReceipt]),
    /files\.retrieveMetadata/u,
    'R2',
  );
});

test('path placeholders match one non-empty segment and no broader route', () => {
  // Boundary partition: one encoded ID is valid; missing, extra and nested
  // path segments are invalid. This prevents a nearby endpoint from producing
  // a false receipt for the owned operation.
  assert.equal(receiptMatchesOperation(betaReceipt, betaOperation), true);
  for (const path of ['/v1/files/', '/v1/files', '/v1/files/a/extra']) {
    assert.equal(receiptMatchesOperation({ ...betaReceipt, path }, betaOperation), false, path);
  }
});

test('transport hook records only non-secret completed exchange coordinates', () => {
  // Information-flow rule: credentials and bodies may enter the request, but
  // only method/path/selectors/SDK marker/status may enter the receipt. The
  // response status is written after fetch completes, so an attempted request
  // cannot leave an apparently successful ownership fact.
  const directory = mkdtempSync(resolve(tmpdir(), 'awaken-receipt-hook-test-'));
  const receiptFile = resolve(directory, 'receipt.jsonl');
  try {
    execFileSync(process.execPath, [
      '--import', resolve(import.meta.dirname, 'managed_sdk_receipt_hook.mjs'),
      '--input-type=module',
      '--eval',
      "await fetch('data:application/json,%7B%7D', { headers: {"
        + " 'x-api-key': 'must-not-leak', 'x-stainless-lang': 'js',"
        + " 'anthropic-beta': 'one,two' } })",
    ], {
      env: { ...process.env, AWAKEN_MANAGED_SDK_RECEIPT_FILE: receiptFile },
    });
    const serialized = readFileSync(receiptFile, 'utf8');
    assert.ok(!serialized.includes('must-not-leak'), 'credential non-interference');
    assert.deepEqual(JSON.parse(serialized), {
      method: 'GET',
      path: 'application/json,%7B%7D',
      beta: null,
      betas: ['one', 'two'],
      sdk: true,
      status: 200,
    });
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('finite receipt model accepts exactly the conjunction of all ownership coordinates', () => {
  // Finite model check over the six independent predicates in the ownership
  // invariant. Exhausting 2^6 combinations proves no single missing coordinate
  // or interaction of missing coordinates can satisfy the matcher accidentally.
  const dimensions = [true, false];
  let cases = 0;
  for (const sdk of dimensions) {
    for (const healthy of dimensions) {
      for (const method of dimensions) {
        for (const path of dimensions) {
          for (const selector of dimensions) {
            for (const capability of dimensions) {
              const receipt = {
                ...betaReceipt,
                sdk,
                status: healthy ? 200 : 500,
                method: method ? 'GET' : 'POST',
                path: path ? '/v1/files/file_1' : '/v1/files/file_1/extra',
                beta: selector ? 'true' : null,
                betas: capability ? ['files-api-2025-04-14'] : [],
              };
              assert.equal(
                receiptMatchesOperation(receipt, betaOperation),
                sdk && healthy && method && path && selector && capability,
                JSON.stringify(receipt),
              );
              cases += 1;
            }
          }
        }
      }
    }
  }
  assert.equal(cases, 64);
});
