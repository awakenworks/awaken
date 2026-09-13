import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { extractOperationsFromPackageRoot } from '../src/extract-operations.mjs';
import { extractRequestContractsFromPackageRoot } from '../src/extract-wire-contracts.mjs';
import {
  HEX64_WITNESS,
  INFERENCE_GEO_WITNESS,
  MAP_KEY_WITNESS,
  STRING_WITNESS,
  TIMESTAMP_WITNESS,
  maximalValue,
  minimalValue,
  officialRequestWitnesses,
  representativeString,
  serializedRequestWitnesses,
  unionAlternatives,
  witnessInvocations,
} from '../src/official-request-witnesses.mjs';
import { resolveSdkPackage } from '../src/package-source.mjs';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const scope = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/scope.json')));

const literal = (value) => ({ kind: 'literal', primitive: typeof value, value });
const contract = {
  parameters: [
    { name: 'resourceID', required: true, value: { kind: 'string' } },
    {
      name: 'params',
      required: false,
      value: {
        kind: 'object',
        additional: false,
        properties: {
          content_sha256: { required: true, value: { kind: 'string' } },
          'created_at[gte]': { required: false, value: { kind: 'string' } },
          mode: {
            required: true,
            value: { kind: 'union', variants: [literal('one'), literal('two')] },
          },
          nullable: {
            required: false,
            value: { kind: 'union', variants: [{ kind: 'null' }, { kind: 'number' }] },
          },
          precondition: {
            required: false,
            value: {
              kind: 'union',
              variants: [
                {
                  kind: 'object',
                  additional: false,
                  properties: {
                    type: { required: true, value: literal('content_sha256') },
                    content_sha256: { required: true, value: { kind: 'string' } },
                  },
                },
                {
                  kind: 'object',
                  additional: false,
                  properties: {
                    type: { required: true, value: literal('other') },
                    flag: {
                      required: true,
                      value: { kind: 'union', variants: [literal(true), literal(false)] },
                    },
                  },
                },
              ],
            },
          },
          tags: { required: false, value: { kind: 'array', item: { kind: 'string' } } },
          metadata: {
            required: false,
            value: { kind: 'object', properties: {}, additional: { kind: 'string' } },
          },
        },
      },
    },
  ],
};

test('minimal and maximal witnesses are derived only from the declaration', () => {
  // Cause/effect: required-only -> minimal carries exactly the required
  // fields with the first non-null union member; every optional present ->
  // maximal carries all fields, one array item, one map key. Field names
  // select formats only for bare strings (hash / timestamp).
  const params = contract.parameters[1].value;
  assert.deepEqual(minimalValue(params, ['params']), {
    content_sha256: HEX64_WITNESS,
    mode: 'one',
  });
  assert.deepEqual(maximalValue(params, ['params']), {
    content_sha256: HEX64_WITNESS,
    'created_at[gte]': TIMESTAMP_WITNESS,
    mode: 'one',
    nullable: 1,
    precondition: { type: 'content_sha256', content_sha256: HEX64_WITNESS },
    tags: [STRING_WITNESS],
    metadata: { [MAP_KEY_WITNESS]: STRING_WITNESS },
  });
  assert.equal(representativeString(['params', 'expected_content_sha256']), HEX64_WITNESS);
  assert.equal(representativeString(['params', 'updated_at']), TIMESTAMP_WITNESS);
  assert.equal(representativeString(['params', 'model', 'inference_geo']), INFERENCE_GEO_WITNESS);
  assert.equal(representativeString(['params', 'path']), STRING_WITNESS);
});

test('every union member reachable beneath the maximal value gets one witness', () => {
  // Decision table over the alternatives of `params`:
  // | at                | label                          | effect |
  // | mode              | mode.two                       | second literal |
  // | nullable          | nullable.null                  | explicit null |
  // | precondition      | precondition.other             | second object member |
  // | precondition      | precondition.other.flag.false  | member nested in the alternative |
  const alternatives = unionAlternatives(contract.parameters[1].value, ['params']);
  const byLabel = new Map(alternatives.map((alternative) => [alternative.label, alternative]));
  assert.deepEqual([...byLabel.keys()].sort(), [
    'mode.two',
    'nullable.null',
    'precondition.other',
    'precondition.other.flag.false',
  ]);
  assert.deepEqual(byLabel.get('mode.two'), { at: ['mode'], label: 'mode.two', value: 'two' });
  assert.deepEqual(byLabel.get('nullable.null').value, null);
  assert.deepEqual(byLabel.get('precondition.other').value, { type: 'other', flag: true });
  assert.deepEqual(byLabel.get('precondition.other.flag.false').value, {
    type: 'other',
    flag: false,
  });

  const invocations = witnessInvocations(contract);
  assert.deepEqual(invocations.map(({ id }) => id), [
    'minimal',
    'maximal',
    'params.mode.two',
    'params.nullable.null',
    'params.precondition.other',
    'params.precondition.other.flag.false',
  ]);
  assert.deepEqual(invocations[0].invocation, [STRING_WITNESS]);
  assert.equal(invocations[5].invocation[1].precondition.flag, false);
  assert.equal(invocations[5].invocation[1].mode, 'one', 'one change from maximal');
});

test('the current official SDK emits the official memory precondition and delete query', async () => {
  // The c4f378c08 regression rows, asserted against the real current package:
  // the update body precondition is `{type:"content_sha256", content_sha256}`
  // and the conditional delete is the `expected_content_sha256` query.
  const { root, version } = resolveSdkPackage('@anthropic-ai/sdk-current');
  const sdk = extractOperationsFromPackageRoot(root, scope);
  const operations = sdk.operations.filter(({ id }) =>
    id === 'beta.memoryStores.memories.update' || id === 'beta.memoryStores.memories.delete');
  const contracts = extractRequestContractsFromPackageRoot(
    root,
    scope,
    sdk.operations.map(({ id }) => id),
  );
  const artifact = await officialRequestWitnesses({ packageRoot: root, version, operations, contracts });
  assert.equal(artifact.current_oracle, version);
  const update = artifact.operations['beta.memoryStores.memories.update'];
  const maximal = update.witnesses.find(({ id }) => id === 'maximal');
  assert.equal(maximal.method, 'POST');
  assert.equal(maximal.path, `/v1/memory_stores/${STRING_WITNESS}/memories/${STRING_WITNESS}`);
  assert.deepEqual(maximal.body.value.precondition, {
    type: 'content_sha256',
    content_sha256: HEX64_WITNESS,
  });
  assert.equal(update.witnesses[0].id, 'minimal');
  assert.deepEqual(update.witnesses[0].body, { kind: 'json', value: {} });
  const remove = artifact.operations['beta.memoryStores.memories.delete'];
  const removeMaximal = remove.witnesses.find(({ id }) => id === 'maximal');
  assert.deepEqual(removeMaximal.query, [
    ['beta', 'true'],
    ['expected_content_sha256', HEX64_WITNESS],
  ]);
  assert.deepEqual(removeMaximal.body, { kind: 'empty' });
  // Header-only variants (`betas`) collapse onto the same wire identity.
  assert.ok(remove.witnesses.every(({ id }) => !id.startsWith('params.betas')));

  const serialized = serializedRequestWitnesses(artifact);
  assert.equal(serialized, serializedRequestWitnesses(JSON.parse(serialized)), 'stable');
  assert.match(serialized, /^ {8}\{"anthropic_beta":/mu, 'one witness per line');
});
