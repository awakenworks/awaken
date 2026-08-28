import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, realpathSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { resolveSdkPackage } from '../../packages/managed-sdk-oracle/src/package-source.mjs';

const hook = path.resolve(import.meta.dirname, 'managed_sdk_package_hook.mjs');

function executeSelection(moduleName, expectedVersion = undefined) {
  const sdk = resolveSdkPackage(moduleName);
  const directory = mkdtempSync(path.resolve(tmpdir(), 'awaken-managed-sdk-selection-'));
  const resolutionFile = path.resolve(directory, 'resolution.jsonl');
  const result = spawnSync(process.execPath, [
    '--import', hook,
    '--input-type=module',
    '--eval',
    "await import('@anthropic-ai/sdk');",
  ], {
    encoding: 'utf8',
    env: {
      ...process.env,
      AWAKEN_MANAGED_SDK_PACKAGE_ROOT: sdk.root,
      AWAKEN_MANAGED_SDK_PACKAGE_VERSION: expectedVersion ?? sdk.version,
      AWAKEN_MANAGED_SDK_RESOLUTION_FILE: resolutionFile,
    },
  });
  return { directory, resolutionFile, result, sdk };
}

test('canonical imports resolve inside every selected exact SDK package', () => {
  // Differential causal graph: C1 select the current root; C2 select the
  // reviewed candidate root; C3 application source imports the same canonical
  // package name. E1 each process evaluates a file inside only its selected
  // root and records the exact manifest version. This proves candidate replay
  // does not require source edits, copied tests, or mutable node_modules state.
  for (const moduleName of ['@anthropic-ai/sdk-current', '@anthropic-ai/sdk-candidate']) {
    const execution = executeSelection(moduleName);
    try {
      assert.equal(execution.result.status, 0, execution.result.stderr);
      const records = readFileSync(execution.resolutionFile, 'utf8')
        .trim()
        .split('\n')
        .map((line) => JSON.parse(line));
      assert.ok(records.length > 0);
      assert.ok(records.every(({ version }) => version === execution.sdk.version));
      assert.ok(records.every(({ url }) => {
        const relative = path.relative(execution.sdk.root, realpathSync(new URL(url)));
        return !relative.startsWith('..') && !path.isAbsolute(relative);
      }));
    } finally {
      rmSync(execution.directory, { recursive: true, force: true });
    }
  }
});

test('package selection rejects an adjacent expected version before application import', () => {
  // Fault injection: preserve the reviewed candidate bytes/root while claiming
  // the baseline version. The preload must terminate before canonical package
  // resolution, closing the false-evidence path at the process boundary.
  const execution = executeSelection('@anthropic-ai/sdk-candidate', '0.121.0');
  try {
    assert.notEqual(execution.result.status, 0);
    assert.match(execution.result.stderr, /does not match expected/u);
  } finally {
    rmSync(execution.directory, { recursive: true, force: true });
  }
});
