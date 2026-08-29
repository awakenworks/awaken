import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  operationCoverage,
  RESOURCE_RESTART_EVIDENCE,
  resourceOf,
  surfaceFixture,
} from '../src/coverage.mjs';
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
  assert.deepEqual(
    new Set(Object.keys(RESOURCE_RESTART_EVIDENCE)),
    new Set(Object.keys(config.resources)),
    'every operation resource has exactly one process-replacement owner',
  );
  for (const row of rows) {
    assert.equal(
      row.evidence.deployed_route_semantics.case_id,
      `${row.id}.deployed.actual-reference`,
    );
    assert.match(
      row.evidence.deployed_route_semantics.owner,
      /conformance\/deployed-sweep\.mjs$/u,
    );
    assert.match(
      row.evidence.local_route_boundary.owner,
      /conformance\/managed_local_operation_sweep_e2e\.mjs$/u,
    );
    if (!row.id.startsWith('documented.')) {
      assert.equal(row.evidence.sdk_transport.case_id, `${row.id}.transport.current`);
      assert.match(row.evidence.sdk_transport.owner, /sdk-wire-lifecycle\.test\.ts$/u);
    } else {
      assert.equal(
        row.evidence.sdk_transport.not_applicable,
        'sdk_absent_documented_route',
      );
    }
    assert.match(row.evidence.rust_behavior.owner, /\.rs$/u);
    assert.match(row.evidence.rust_behavior.case_id, /^[a-z][a-z0-9_]+$/u);
    assert.deepEqual(
      row.evidence.resource_restart_semantics,
      RESOURCE_RESTART_EVIDENCE[row.resource],
    );
  }

  for (const [resource, evidence] of Object.entries(RESOURCE_RESTART_EVIDENCE)) {
    const source = fs.readFileSync(path.join(repoRoot, evidence.owner), 'utf8');
    assert.match(
      source,
      new RegExp(`Test design: ${evidence.case_id}\\b`, 'u'),
      `${resource} recovery owner contains its named test design`,
    );
  }

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

test('each operation owns one exact executable Rust behavior case', () => {
  // Cause/effect graph: C1 an operation has no matching case, C2 two cases
  // overlap, C3 the named function is absent/not a test, or C4 a configured
  // pattern matches no operation. Each cause must fail closed. This prevents a
  // broad source-file marker from being mistaken for per-operation evidence.
  const oneOperation = [{
    id: 'beta.files.list', method: 'GET', path: '/v1/files',
  }];
  const designed = (name) => `// Test design: ${name}\n`
    + '// Cause/effect graph: operation -> observable behavior.\n'
    + '// Decision table: valid -> accept; invalid -> reject.\n'
    + `#[tokio::test]\nasync fn ${name}() {}`;
  const run = (resources, readText = () => designed('list_files')) =>
    operationCoverage({
      extracted: [{ role: 'current_oracle', operations: oneOperation }],
      documentedRoutes: [],
      config: { schema_version: 2, resources },
      repoRoot,
      readText,
    });

  assert.throws(
    () => run({ files: { rust_behavior_cases: [{
      path: 'files.rs', test: 'list_files', operations: ['beta.files.upload'],
    }] } }),
    /must have exactly one Rust behavior case; found 0/u,
    'C1',
  );
  assert.throws(
    () => run({ files: { rust_behavior_cases: [
      { path: 'files.rs', test: 'list_files', operations: ['beta.files.*'] },
      { path: 'files.rs', test: 'list_files_again', operations: ['beta.files.list'] },
    ] } }, () => `${designed('list_files')}\n${designed('list_files_again')}`),
    /must have exactly one Rust behavior case; found 2/u,
    'C2',
  );
  assert.throws(
    () => run({ files: { rust_behavior_cases: [{
      path: 'files.rs', test: 'list_files', operations: ['beta.files.list'],
    }] } }, () => 'async fn list_files() {}'),
    /contains no behavior test list_files/u,
    'C3',
  );
  assert.throws(
    () => run({ files: { rust_behavior_cases: [{
      path: 'files.rs',
      test: 'list_files',
      operations: ['beta.files.list', 'beta.files.upload'],
    }] } }),
    /dead operation pattern beta\.files\.upload/u,
    'C4',
  );
  assert.throws(
    () => run({ files: { rust_behavior_cases: [{
      path: 'files.rs', test: 'list_files', operations: ['beta.files.list'],
    }] } }, () => '#[test]\nfn list_files() {}'),
    /has no adjacent named test design/u,
    'a behavior owner without its causal design cannot certify an operation',
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
