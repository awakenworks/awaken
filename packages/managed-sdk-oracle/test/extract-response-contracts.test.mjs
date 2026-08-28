import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
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

  const openPurposes = new Set();
  const visit = (value) => {
    if (!value || typeof value !== 'object') return;
    assert.notEqual(value.kind, 'any', 'unnamed open response JSON is impossible');
    assert.notEqual(value.kind, 'recursive', 'unchecked recursive response JSON is impossible');
    if (value.kind === 'open-json') openPurposes.add(value.purpose);
    for (const nested of Object.values(value)) visit(nested);
  };
  for (const contract of Object.values(contracts)) visit(contract);
  assert.deepEqual(
    openPurposes,
    new Set(['json-schema', 'tool-input']),
    'the official dynamic response boundary has exactly two named intents',
  );
});

test('only intent-named open JSON and finite response types can produce behavior evidence', () => {
  // Cause/effect graph: C1 an adjacent official declaration introduces `any`
  // or `unknown`; C2 it introduces a recursive JSON type. Neither has a finite
  // structural oracle or one of the two official extension intents. Effects:
  // E1 extraction fails before a real-process owner can claim compatibility;
  // E2 a future SDK change requires an explicit, reviewable validator instead
  // of silently accepting every response. Decision table: bounded finite DTO
  // or named JSON-Schema/tool-input extension -> extract; C1 -> E1; C2 -> E1.
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'managed-response-contract-'));
  try {
    fs.mkdirSync(path.join(root, 'resources/beta'), { recursive: true });
    fs.mkdirSync(path.join(root, 'resources'), { recursive: true });
    fs.writeFileSync(
      path.join(root, 'package.json'),
      JSON.stringify({ name: '@anthropic-ai/sdk', version: '0.0.0-test' }),
    );
    fs.writeFileSync(path.join(root, 'resources/beta/files.js'), 'export class Files {}\n');
    const declaration = path.join(root, 'resources/beta/files.d.ts');
    const extract = () => extractResponseContractsFromPackageRoot(
      root,
      { beta_resource_roots: ['files'], ga_resource_roots: [] },
      ['beta.files.retrieve'],
    );

    fs.writeFileSync(declaration, [
      'interface APIPromise<T> extends Promise<T> {}',
      'export declare class Files {',
      '  retrieve(): APIPromise<{ payload: unknown }>;',
      '}',
    ].join('\n'));
    assert.throws(extract, /unreviewed open JSON.*payload/u, 'C1/E1');

    fs.writeFileSync(declaration, [
      'interface APIPromise<T> extends Promise<T> {}',
      'interface Node { next: Node | null }',
      'export declare class Files {',
      '  retrieve(): APIPromise<Node>;',
      '}',
    ].join('\n'));
    assert.throws(extract, /recursive type.*next/u, 'C2/E1');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
