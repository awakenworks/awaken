import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { extractOperations } from '../src/extract-operations.mjs';
import {
  auditRequestTypesFromPackageRoot,
  extractRequestContractsFromPackageRoot,
  extractResponseContractsFromPackageRoot,
} from '../src/extract-wire-contracts.mjs';
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

function requestBoundariesFor(module) {
  const operations = extractOperations(module, scope).operations;
  return auditRequestTypesFromPackageRoot(
    resolveSdkPackage(module).root,
    scope,
    operations.map(({ id }) => id),
  );
}

function requestContractsFor(module) {
  const operations = extractOperations(module, scope).operations;
  return {
    operations,
    contracts: extractRequestContractsFromPackageRoot(
      resolveSdkPackage(module).root,
      scope,
      operations.map(({ id }) => id),
    ),
  };
}

test('every supported and candidate request type has only intent-named open JSON', () => {
  // Cause/effect graph: C1 every generated HTTP operation contributes all of
  // its non-transport TypeScript parameters; C2 the TypeChecker follows their
  // complete property/union/index closure; C3 multipart Uploadable remains a
  // named transport boundary. Effects: E1 only custom-tool `input_schema` may
  // contain open JSON, E2 every other any/unknown or unconstrained object fails
  // before the wire exemplar or server can provide evidence, and E3 renaming a
  // body parameter to `options` cannot evade the audit. Historical 0.105 lacks
  // the inline Session-create tool branch, hence six instead of eight named
  // index positions; all later anchors and the candidate own exactly eight.
  const expectedCounts = new Map([
    ['@anthropic-ai/sdk-oldest', 6],
    ['@anthropic-ai/sdk-user-profiles-legacy', 8],
    ['@anthropic-ai/sdk-current', 8],
    ['@anthropic-ai/sdk-candidate', 8],
  ]);
  for (const [module, expectedCount] of expectedCounts) {
    const boundaries = requestBoundariesFor(module);
    assert.equal(boundaries.length, expectedCount, module);
    assert.deepEqual(new Set(boundaries.map(({ purpose }) => purpose)), new Set(['json-schema']));
    for (const boundary of boundaries) {
      assert.ok(boundary.path.includes('input_schema'), `${module}: ${boundary.path.join('.')}`);
    }
  }
});

test('every supported and candidate SDK operation owns one finite request contract', () => {
  // Causal graph: C1 generated JS contributes the exact HTTP-operation set;
  // C2 the adjacent official declaration contributes every non-transport
  // parameter and its complete finite type closure; C3 Uploadable and the
  // intent-named JSON-Schema index remain explicit boundary kinds. Effects:
  // E1 every operation owns exactly one request contract; E2 optionality,
  // nullability, literals, arrays and nested fields cannot disappear; E3 a
  // future unsafe/open request type fails before any witness can be generated.
  for (const { module, version } of anchors.anchors) {
    const { operations, contracts } = requestContractsFor(module);
    assert.equal(Object.keys(contracts).length, operations.length, version);
    assert.deepEqual(Object.keys(contracts), operations.map(({ id }) => id), version);
  }
  const candidate = requestContractsFor('@anthropic-ai/sdk-candidate');
  assert.equal(Object.keys(candidate.contracts).length, candidate.operations.length);
  assert.deepEqual(Object.keys(candidate.contracts), candidate.operations.map(({ id }) => id));
});

test('request extraction preserves method parameters, optionality, unions, and upload boundaries', () => {
  // Orthogonal representatives pin the extractor grammar while the inventory
  // test above proves breadth. Mutating any of these nodes changes the generated
  // witness graph instead of being hidden by a required-only call sweep.
  const { contracts } = requestContractsFor('@anthropic-ai/sdk-candidate');
  const create = contracts['beta.sessions.create'];
  assert.deepEqual(create.parameters.map(({ name, required }) => [name, required]), [
    ['params', true],
  ]);
  const agent = create.parameters[0].value.properties.agent;
  assert.equal(agent.required, true);
  assert.equal(agent.value.kind, 'union');
  assert.ok(agent.value.variants.some(({ kind }) => kind === 'string'));
  assert.ok(agent.value.variants.some(({ kind }) => kind === 'object'));
  assert.equal(
    create.parameters[0].value.properties.title.required,
    false,
    'optional body field remains optional',
  );

  const retrieve = contracts['beta.sessions.retrieve'];
  assert.deepEqual(retrieve.parameters.map(({ name, required }) => [name, required]), [
    ['sessionID', true],
    ['params', false],
  ]);
  assert.equal(retrieve.parameters[1].value.kind, 'union');
  assert.ok(retrieve.parameters[1].value.variants.some(({ kind }) => kind === 'null'));

  assert.equal(
    contracts['beta.files.upload'].parameters[0].value.properties.file.value.kind,
    'upload',
  );
  assert.deepEqual(
    contracts['beta.skills.create'].parameters[0].value.properties.files.value,
    { kind: 'array', item: { kind: 'upload' } },
    'Array<Uploadable> retains cardinality instead of collapsing to one file',
  );
  assert.equal(
    contracts['beta.agents.create'].parameters[0].value.properties.tools.value.item.variants
      .find((variant) => variant.kind === 'object'
        && variant.properties.type.value.value === 'custom')
      .properties.input_schema.value.additional.kind,
    'open-json',
  );
});

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
      'export declare class Agents {',
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

test('request audit fails closed before a type escape can reach transport evidence', () => {
  // Decision table: a named JSON-Schema index is the sole open request cell and
  // is admitted; an arbitrary unknown field, `{}`/object escape, or a payload
  // disguised with the transport parameter name `options` is rejected. This is
  // a generator-boundary test: no hand-authored request exemplar can mask an
  // unsafe future declaration because extraction runs over the official graph.
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'managed-request-audit-'));
  try {
    fs.mkdirSync(path.join(root, 'resources/beta'), { recursive: true });
    fs.writeFileSync(
      path.join(root, 'package.json'),
      JSON.stringify({ name: '@anthropic-ai/sdk', version: '0.0.0-test' }),
    );
    fs.writeFileSync(path.join(root, 'resources/beta/agents.js'), 'export class Agents {}\n');
    const declaration = path.join(root, 'resources/beta/agents.d.ts');
    const audit = () => auditRequestTypesFromPackageRoot(
      root,
      { beta_resource_roots: ['agents'], ga_resource_roots: [] },
      ['beta.agents.create'],
    );
    const writeMethod = (parameter) => fs.writeFileSync(declaration, [
      'interface APIPromise<T> extends Promise<T> {}',
      'interface RequestOptions { headers?: Record<string, string> }',
      'type Uploadable = Uint8Array;',
      'export declare class Files {',
      `  create(${parameter}): APIPromise<{ id: string }>;`,
      '}',
    ].join('\n'));

    writeMethod('params: { tools: Array<{ input_schema: { [key: string]: unknown } }> }');
    assert.deepEqual(audit(), [{
      operation: 'beta.agents.create',
      path: ['params', 'tools', '[]', 'input_schema', '*'],
      purpose: 'json-schema',
    }], 'named open JSON is admitted');

    writeMethod('params: { file: Uploadable }');
    assert.deepEqual(audit(), [], 'exact multipart upload is a distinct transport boundary');
    writeMethod('params: { file: Uploadable | { payload: unknown } }');
    assert.throws(audit, /unreviewed open JSON.*payload/u, 'multipart cannot hide open JSON');

    writeMethod('options?: RequestOptions');
    assert.deepEqual(audit(), [], 'exact SDK transport options are outside the wire body');
    writeMethod('options: RequestOptions | unknown');
    assert.throws(audit, /unreviewed open JSON.*options/u, 'transport options cannot hide open JSON');

    writeMethod('params: { payload: unknown }');
    assert.throws(audit, /unreviewed open JSON.*payload/u, 'unknown fails closed');
    writeMethod('params: { payload: object }');
    assert.throws(audit, /unconstrained object.*payload/u, 'object escape fails closed');
    writeMethod('options: unknown');
    assert.throws(audit, /unreviewed open JSON.*options/u, 'parameter spelling cannot evade audit');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
