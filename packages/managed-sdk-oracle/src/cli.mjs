import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { extractOperations } from './extract-operations.mjs';
import { stableJson } from './normalize.mjs';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repoRoot = path.resolve(packageRoot, '../..');
const anchors = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/anchors.json')));
const scope = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/scope.json')));
const oraclePath = path.join(repoRoot, 'contracts/anthropic-managed/upstream-oracle.generated.json');
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
    assert.ok(sdk.operations.length > 0, `${anchor.id} extracted no Managed operations`);
    return {
      id: anchor.id,
      role: anchor.role,
      version: sdk.version,
      operation_fingerprint: fingerprint(sdk.operations),
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
      supported_sdk_anchors: extracted.map(({ id, role, version, operation_fingerprint }) => ({
        id,
        role,
        version,
        operation_fingerprint,
      })),
    }),
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
  console.log(`Generated Managed SDK oracle for ${output.support.current_oracle}`);
} else if (command === 'check') {
  check(oraclePath, output.oracle);
  check(supportPath, output.support);
  console.log(`Managed SDK oracle is current at ${output.support.current_oracle}`);
} else {
  throw new Error('Usage: node src/cli.mjs <generate|check>');
}
