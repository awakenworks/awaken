import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { extractOperations } from '../src/extract-operations.mjs';
import { extractResponseContractsFromPackageRoot } from '../src/extract-response-contracts.mjs';
import { resolveSdkPackage } from '../src/package-source.mjs';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const scope = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/scope.json'), 'utf8'));
const anchors = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/anchors.json'), 'utf8'));

function contractsFor(module) {
  const operations = extractOperations(module, scope).operations;
  return {
    operations,
    contracts: extractResponseContractsFromPackageRoot(
      resolveSdkPackage(module).root,
      scope,
      operations.map(({ id }) => id),
    ),
  };
}

test('every supported and candidate SDK operation owns one declaration-derived response contract', () => {
  // Cause/effect graph: C1 each exact npm anchor supplies generated JS routes;
  // C2 the adjacent official .d.ts supplies its response wrapper and payload;
  // C3 extraction intersects those two independent inventories. Effects: E1
  // every operation has exactly one contract, E2 no declaration-only helper
  // enters the HTTP claim, and E3 JSON/page, binary, and stream partitions all
  // remain explicit. This runs over every admitted historical/current anchor;
  // the candidate is independently exercised below because it is intentionally
  // not yet a supported anchor.
  for (const { module, version } of anchors.anchors) {
    const { operations, contracts } = contractsFor(module);
    assert.equal(Object.keys(contracts).length, operations.length, version);
    assert.deepEqual(Object.keys(contracts), operations.map(({ id }) => id), version);
    assert.ok(Object.values(contracts).some(({ kind }) => kind === 'json'), `${version}: JSON`);
    assert.ok(Object.values(contracts).some(({ kind }) => kind === 'binary'), `${version}: binary`);
    assert.ok(Object.values(contracts).some(({ kind }) => kind === 'stream'), `${version}: stream`);
  }

  const candidate = contractsFor('@anthropic-ai/sdk-candidate');
  assert.equal(Object.keys(candidate.contracts).length, candidate.operations.length, 'candidate');
  assert.deepEqual(Object.keys(candidate.contracts), candidate.operations.map(({ id }) => id));
});

test('official response extraction preserves closed fields, nesting, nullability, pages, unions, and media', () => {
  // Orthogonal partitions and expected effects:
  // R1 ordinary JSON -> exact closed root/nested properties;
  // R2 required nullable -> required property whose value union includes null;
  // R3 optional -> non-required property without fabricated null;
  // R4 PagePromise -> raw public page fields only, never SDK internals/methods;
  // R5 response union -> every official variant retained;
  // R6 Response/Stream -> binary/SSE classifications that instrumentation does
  // not parse or consume. A single representative from each partition pins the
  // extractor grammar while the preceding total-inventory test closes breadth.
  const { contracts } = contractsFor('@anthropic-ai/sdk-current');
  const session = contracts['beta.sessions.create'].schema;
  assert.equal(session.kind, 'object', 'R1');
  assert.deepEqual(Object.keys(session.properties), [
    'agent', 'archived_at', 'budget', 'created_at', 'deployment_id',
    'environment_id', 'id', 'metadata', 'outcome_evaluations', 'resources',
    'stats', 'status', 'title', 'type', 'updated_at', 'usage', 'vault_ids',
  ], 'R1');
  assert.deepEqual(Object.keys(session.properties.agent.value.properties), [
    'description', 'id', 'mcp_servers', 'model', 'multiagent', 'name', 'skills',
    'system', 'tools', 'type', 'version',
  ], 'R1 nested');
  assert.deepEqual(
    session.properties.type.value,
    { kind: 'literal', primitive: 'string', value: 'session' },
    'R1 generated wire discriminator remains finite',
  );
  assert.equal(session.properties.archived_at.required, true, 'R2');
  assert.deepEqual(
    session.properties.archived_at.value.variants.map(({ kind }) => kind),
    ['null', 'string'],
    'R2',
  );
  assert.equal(session.properties.deployment_id.required, false, 'R3');

  assert.deepEqual(
    Object.keys(contracts['beta.sessions.list'].schema.properties),
    ['data', 'next_page', 'prev_page'],
    'R4',
  );
  assert.deepEqual(
    contracts['beta.sessions.list'].evidence,
    { nonEmptyArrays: [['data']] },
    'R4 empty pages cannot vacuously certify the element DTO',
  );
  assert.deepEqual(
    contracts['beta.environments.work.poll'].schema.variants.map(({ kind }) => kind),
    ['null', 'object'],
    'R5 nullable response',
  );
  assert.equal(contracts['beta.sessions.resources.retrieve'].schema.variants.length, 3, 'R5 tagged union');
  assert.deepEqual(
    contracts['beta.sessions.resources.retrieve'].schema.variants
      .map(({ properties }) => properties.type.value.value)
      .sort(),
    ['file', 'github_repository', 'memory_store'],
    'R5 every tagged-union discriminator remains finite',
  );
  assert.deepEqual(contracts['files.download'], { kind: 'binary' }, 'R6');
  assert.deepEqual(contracts['beta.sessions.events.stream'], { kind: 'stream' }, 'R6');
});
