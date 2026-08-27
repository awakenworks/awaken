import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { operationCoverage, resourceOf, surfaceFixture } from '../src/coverage.mjs';
import { extractOperations } from '../src/extract-operations.mjs';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repoRoot = path.resolve(packageRoot, '../..');
const anchors = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/anchors.json')));
const scope = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/scope.json')));
const config = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/coverage.json')));
const extracted = anchors.anchors.map((anchor) => ({
  ...anchor,
  operations: extractOperations(anchor.module, scope).operations,
}));

test('coverage ledger closes every current and documented operation exactly once', () => {
  // Cause/effect graph: C1 official current SDK operations plus C2 reviewed
  // SDK-absent routes produce E1 one evidence-bearing ledger row each. C3 an
  // unknown resource or C4 duplicate operation must fail closed rather than
  // silently becoming an untested compatibility claim.
  const rows = operationCoverage({
    extracted,
    documentedRoutes: scope.documented_routes,
    config,
    repoRoot,
  });
  const expected = extracted.find(({ role }) => role === 'current_oracle').operations.length
    + scope.documented_routes.length;
  assert.equal(rows.length, expected, 'C1+C2/E1');
  assert.equal(new Set(rows.map(({ id }) => id)).size, expected, 'C1+C2/E1');
  assert.deepEqual(
    new Set(rows.map(({ resource }) => resource)),
    new Set(Object.keys(config.resources)),
    'every configured resource owns live operations',
  );

  assert.throws(
    () => operationCoverage({
      extracted: [{ role: 'current_oracle', operations: [{ id: 'beta.unknown.list' }] }],
      documentedRoutes: [],
      config,
      repoRoot,
    }),
    /has no resource coverage owner/u,
    'C3',
  );
  assert.throws(
    () => operationCoverage({
      extracted: [{ role: 'current_oracle', operations: [
        { id: 'beta.files.list' },
        { id: 'beta.files.list' },
      ] }],
      documentedRoutes: [],
      config,
      repoRoot,
    }),
    /duplicate covered operation/u,
    'C4',
  );
});

test('coverage owner must point to executable Rust behavior tests', () => {
  // Cause/effect graph: C1 a route with an owner but C2 no executable test
  // marker must produce E1 a generator failure. A source-file reference alone
  // is not accepted as behavioral evidence.
  assert.throws(
    () => operationCoverage({
      extracted: [{ role: 'current_oracle', operations: [{
        id: 'beta.files.list', method: 'GET', path: '/v1/files',
      }] }],
      documentedRoutes: [],
      config,
      repoRoot,
      readText: () => 'fn helper() {}',
    }),
    /contains no behavior tests/u,
    'C1+C2/E1',
  );
});

test('surface fixture binds the exact SDK module and every operation as callable', () => {
  // Cause/effect graph: C1 exact package alias and C2 discovered operation set
  // produce E1 a strict compile fixture. Missing modules, renamed methods, or a
  // non-callable member therefore fail TypeScript compilation.
  const fixture = surfaceFixture({
    module: '@anthropic-ai/sdk-current',
    operations: [{ id: 'beta.sessions.create' }, { id: 'beta.files.list' }],
  });
  assert.match(fixture, /from '@anthropic-ai\/sdk-current'/u, 'C1/E1');
  assert.match(fixture, /client\.beta\.sessions\.create/u, 'C2/E1');
  assert.match(fixture, /satisfies readonly Callable\[\]/u, 'C2/E1');
});

test('documented organization tunnel routes share the tunnel bounded context', () => {
  assert.equal(resourceOf({ id: 'documented.organizationTunnels.list' }), 'tunnels');
});
