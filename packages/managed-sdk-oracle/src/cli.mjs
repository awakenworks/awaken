import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { extractOperations } from './extract-operations.mjs';
import { managedTypeFingerprint } from './extract-types.mjs';
import { stableJson } from './normalize.mjs';
import { operationCoverage, surfaceFixture } from './coverage.mjs';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repoRoot = path.resolve(packageRoot, '../..');
const anchors = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/anchors.json')));
const scope = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/scope.json')));
const coverage = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/coverage.json')));
const oraclePath = path.join(repoRoot, 'contracts/anthropic-managed/upstream-oracle.generated.json');
const coveragePath = path.join(
  repoRoot,
  'contracts/anthropic-managed/operation-coverage.generated.json',
);
const supportPath = path.join(
  repoRoot,
  'crates/server/awaken-protocol-managed/contracts/sdk-support.generated.json',
);

function fingerprint(operations) {
  return crypto.createHash('sha256').update(JSON.stringify(stableJson(operations))).digest('hex');
}

function generated() {
  const extracted = anchors.anchors.map((anchor) => {
    const sdk = extractOperations(anchor.module, scope);
    const types = managedTypeFingerprint(anchor.module, scope);
    assert.ok(sdk.operations.length > 0, `${anchor.id} extracted no Managed operations`);
    return {
      id: anchor.id,
      module: anchor.module,
      role: anchor.role,
      version: sdk.version,
      operation_fingerprint: fingerprint(sdk.operations),
      type_fingerprint: types.fingerprint,
      declaration_file_count: types.file_count,
      operations: sdk.operations,
    };
  });
  const current = extracted.filter((anchor) => anchor.role === 'current_oracle');
  assert.equal(current.length, 1, 'exactly one current oracle is required');
  const currentOracle = current[0];
  const currentById = new Map(
    currentOracle.operations.map((operation) => [operation.id, operation]),
  );
  const summaries = extracted.map((anchor) => {
    const anchorIds = new Set(anchor.operations.map((operation) => operation.id));
    return {
      id: anchor.id,
      role: anchor.role,
      version: anchor.version,
      operation_fingerprint: anchor.operation_fingerprint,
      type_fingerprint: anchor.type_fingerprint,
      declaration_file_count: anchor.declaration_file_count,
      only_in_anchor: anchor.operations.filter(({ id }) => !currentById.has(id)),
      only_in_current: currentOracle.operations
        .filter(({ id }) => !anchorIds.has(id))
        .map(({ id }) => id),
    };
  });
  return {
    oracle: stableJson({
      schema_version: 1,
      current: currentOracle,
      anchors: summaries,
      documented_routes: scope.documented_routes,
    }),
    support: stableJson({
      schema_version: 1,
      current_oracle: currentOracle.version,
      supported_sdk_anchors: extracted.map(
        ({
          id,
          role,
          version,
          operation_fingerprint,
          type_fingerprint,
          declaration_file_count,
        }) => ({
          id,
          role,
          version,
          operation_fingerprint,
          type_fingerprint,
          declaration_file_count,
        }),
      ),
    }),
    coverage: stableJson({
      schema_version: 1,
      operations: operationCoverage({
        extracted,
        documentedRoutes: scope.documented_routes,
        config: coverage,
        repoRoot,
      }),
    }),
    fixtures: Object.fromEntries(
      extracted.map((anchor) => [
        path.join(packageRoot, 'fixtures', 'generated', `${anchor.id}.ts`),
        surfaceFixture(anchor),
      ]),
    ),
  };
}

function serialized(value) {
  return `${JSON.stringify(value, null, 2)}\n`;
}

function write(target, value) {
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, serialized(value));
}

function check(target, value) {
  assert.equal(fs.readFileSync(target, 'utf8'), serialized(value), `${target} is stale`);
}

const command = process.argv[2];
const output = generated();
if (command === 'generate') {
  write(oraclePath, output.oracle);
  write(supportPath, output.support);
  write(coveragePath, output.coverage);
  for (const [target, value] of Object.entries(output.fixtures)) {
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(target, value);
  }
  console.log(`Generated Managed SDK oracle for ${output.support.current_oracle}`);
} else if (command === 'check') {
  check(oraclePath, output.oracle);
  check(supportPath, output.support);
  check(coveragePath, output.coverage);
  for (const [target, value] of Object.entries(output.fixtures)) {
    assert.equal(fs.readFileSync(target, 'utf8'), value, `${target} is stale`);
  }
  console.log(`Managed SDK oracle is current at ${output.support.current_oracle}`);
} else {
  throw new Error('Usage: node src/cli.mjs <generate|check>');
}
