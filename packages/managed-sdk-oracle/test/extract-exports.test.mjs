import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { pathToFileURL } from 'node:url';

import {
  managedExportFingerprintFromPackageRoot,
  staticEsmExports,
} from '../src/extract-exports.mjs';
import { resolveSdkPackage } from '../src/package-source.mjs';
import { readSdkMatrix } from '../src/conformance/clients.mjs';

const packageRoot = path.resolve(import.meta.dirname, '..');
const scope = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/scope.json')));

test('static Managed helper exports equal every exact executable anchor surface', async () => {
  // Differential oracle: C1 parse generated ESM without evaluating it; C2
  // import the already-qualified current package. E1 both inventories are
  // identical for every configured entrypoint. This validates the pre-import
  // parser used to reject a candidate that hides a new helper in an old file.
  for (const anchor of readSdkMatrix()) {
    const sdk = resolveSdkPackage(anchor.module);
    const extracted = managedExportFingerprintFromPackageRoot(sdk.root, scope, {
      allowMissing: anchor.role !== 'current_oracle',
    });
    assert.match(extracted.fingerprint, /^[0-9a-f]{64}$/u);
    if (anchor.role === 'current_oracle') assert.equal(extracted.export_count, 33);
    for (const entrypoint of scope.managed_export_entrypoints) {
      const filename = path.join(sdk.root, entrypoint);
      if (!fs.existsSync(filename)) continue;
      const expected = extracted.exports
        .filter((entry) => entry.entrypoint === entrypoint)
        .map(({ name }) => name);
      const module = await import(pathToFileURL(filename));
      assert.deepEqual(
        expected,
        Object.keys(module).sort((left, right) => left.localeCompare(right)),
        `${anchor.id}/${entrypoint}`,
      );
    }
  }
});

test('Managed helper export extraction fails closed for open or malformed surfaces', () => {
  // Fault table: named declarations/re-exports are closed and deterministic;
  // export-star or an unparseable member has no finite local inventory and is
  // rejected before candidate evaluation.
  assert.deepEqual(
    staticEsmExports('export const alpha = 1; export { beta, gamma as delta } from "./x.mjs";'),
    ['alpha', 'beta', 'delta'],
  );
  assert.throws(() => staticEsmExports('export * from "./open.mjs";'), /export \*/u);
  assert.throws(() => staticEsmExports('export { "invalid" };'), /unsupported export/u);
});

test('0.122 helper removal is one explicit source-compatibility change point', () => {
  // Upstream differential: 0.121 publicly re-exported resolveSkillVersion;
  // 0.122 removed only that symbol from the configured Managed helper surface.
  // Exact set subtraction prevents this breaking developer-facing change from
  // hiding inside the already-owned node.mjs content coordinate.
  const current = managedExportFingerprintFromPackageRoot(
    resolveSdkPackage('@anthropic-ai/sdk-current').root,
    scope,
  );
  const candidate = managedExportFingerprintFromPackageRoot(
    resolveSdkPackage('@anthropic-ai/sdk-candidate').root,
    scope,
  );
  const currentIDs = new Set(current.exports.map(({ id }) => id));
  const candidateIDs = new Set(candidate.exports.map(({ id }) => id));
  assert.deepEqual(
    [...currentIDs].filter((id) => !candidateIDs.has(id)),
    ['tools/agent-toolset/node.mjs#resolveSkillVersion'],
  );
  assert.deepEqual([...candidateIDs].filter((id) => !currentIDs.has(id)), []);
});
