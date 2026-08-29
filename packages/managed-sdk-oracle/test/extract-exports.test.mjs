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
import { loadConformanceClients } from '../src/conformance/clients.mjs';

const packageRoot = path.resolve(import.meta.dirname, '..');
const scope = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/scope.json')));

test('static Managed helper exports equal every exact executable anchor surface', async () => {
  // Differential oracle: C1 parse generated ESM without evaluating it; C2
  // import the already-qualified current package. E1 both inventories are
  // identical for every configured entrypoint. This validates the pre-import
  // parser used to reject a candidate that hides a new helper in an old file.
  for (const anchor of await loadConformanceClients()) {
    const sdk = resolveSdkPackage(anchor.module);
    const extracted = managedExportFingerprintFromPackageRoot(sdk.root, scope, {
      allowMissing: anchor.role !== 'current_oracle',
    });
    assert.match(extracted.fingerprint, /^[0-9a-f]{64}$/u);
    if (anchor.role === 'current_oracle') assert.equal(extracted.export_count, 32);
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
  // Grammar partitions: declarations, aliases, default, namespace, and
  // destructuring have closed AST identities; comments/string literals are not
  // syntax. An open export-star or malformed module has no finite local
  // inventory and is rejected before candidate evaluation.
  assert.deepEqual(
    staticEsmExports('export const alpha = 1; export { beta, gamma as delta } from "./x.mjs";'),
    ['alpha', 'beta', 'delta'],
  );
  assert.deepEqual(
    staticEsmExports(`
      const value = 1;
      const source = { alpha: 1, nested: { beta: 2 } };
      export default value;
      export * as helpers from './helpers.mjs';
      export const { alpha, nested: { beta } } = source;
      // export * from './comment.mjs';
      const text = "export * from './string.mjs'";
    `),
    ['alpha', 'beta', 'default', 'helpers'],
  );
  assert.throws(() => staticEsmExports('export * from "./open.mjs";'), /export \*/u);
  assert.throws(() => staticEsmExports('export const = 1;'), /cannot parse/u);
});

test('0.122 helper removal is one explicit source-compatibility change point', () => {
  // Upstream differential: 0.121 publicly re-exported resolveSkillVersion;
  // 0.122 removed only that symbol from the configured Managed helper surface.
  // Exact set subtraction prevents this breaking developer-facing change from
  // hiding inside the already-owned node.mjs content coordinate.
  const legacy = managedExportFingerprintFromPackageRoot(
    resolveSdkPackage('@anthropic-ai/sdk-beta-resources-legacy').root,
    scope,
  );
  const current = managedExportFingerprintFromPackageRoot(
    resolveSdkPackage('@anthropic-ai/sdk-current').root,
    scope,
  );
  const legacyIDs = new Set(legacy.exports.map(({ id }) => id));
  const currentIDs = new Set(current.exports.map(({ id }) => id));
  assert.deepEqual(
    [...legacyIDs].filter((id) => !currentIDs.has(id)),
    ['tools/agent-toolset/node.mjs#resolveSkillVersion'],
  );
  assert.deepEqual([...currentIDs].filter((id) => !legacyIDs.has(id)), []);
});
