import assert from 'node:assert/strict';
import test from 'node:test';

import {
  assertRequestInvocationWitnessCoverage,
  baselineRequestValue,
  requestInvocationWitnesses,
  requestValueWitnesses,
} from '../src/request-witnesses.mjs';

const contract = {
  parameters: [
    { name: 'resourceID', required: true, value: { kind: 'string' } },
    {
      name: 'params',
      required: false,
      value: {
        kind: 'union',
        variants: [
          { kind: 'null' },
          {
            kind: 'object',
            properties: {
              mode: {
                required: true,
                value: {
                  kind: 'union',
                  variants: [
                    { kind: 'literal', primitive: 'string', value: 'one' },
                    { kind: 'literal', primitive: 'string', value: 'two' },
                  ],
                },
              },
              enabled: { required: false, value: { kind: 'boolean' } },
              files: { required: false, value: { kind: 'array', item: { kind: 'upload' } } },
            },
            additional: { kind: 'number' },
          },
        ],
      },
    },
  ],
};

test('request witnesses satisfy one-at-a-time MC/DC closure', () => {
  // Causal graph: required path + omitted optional params is S0; each union,
  // nested property, explicit null, empty/non-empty array, upload and map value
  // changes one decision from S0. Effects: every finite branch has one witness,
  // optional omission remains distinct from null, and no Cartesian-product
  // explosion obscures the causal field. The recursive checker is the coverage
  // oracle; the exact rows below are mutation guards for its critical branches.
  assert.doesNotThrow(() => assertRequestInvocationWitnessCoverage(contract));
  const invocations = requestInvocationWitnesses(contract);
  assert.deepEqual(invocations[0], ['managed fixture /?% ü']);
  assert.ok(invocations.some(([, params]) => params === null), 'explicit null');
  assert.ok(invocations.some(([, params]) => params?.mode === 'one'), 'union one');
  assert.ok(invocations.some(([, params]) => params?.mode === 'two'), 'union two');
  assert.ok(invocations.some(([, params]) => params?.enabled === true), 'optional present');
  assert.ok(invocations.some(([, params]) => params?.enabled === false), 'boolean false');
  assert.ok(invocations.some(([, params]) => params?.files?.length === 0), 'empty array');
  assert.ok(invocations.some(([, params]) => params?.files?.[0]?.__managed_sdk_upload__), 'upload');
  assert.ok(invocations.some(([, params]) => params?.managed_additional_fixture === 1), 'map');
});

test('broad scalar request types retain encoding-relevant partitions', () => {
  // MC/DC mutation guards: a broad string has empty/non-empty transport paths;
  // numbers cross negative/zero/positive truthiness and sign branches; a broad
  // boolean has both decision outcomes. Literal unions remain owned by their
  // exact finite values and do not acquire unrelated broad witnesses.
  assert.deepEqual(requestValueWitnesses({ kind: 'string' }), [
    '',
    'managed fixture /?% ü',
  ]);
  assert.deepEqual(requestValueWitnesses({ kind: 'boolean' }), [true, false]);
  assert.deepEqual(requestValueWitnesses({ kind: 'number' }), [-1, 0, 1]);
  assert.deepEqual(
    requestValueWitnesses({ kind: 'literal', primitive: 'string', value: 'exact' }),
    ['exact'],
  );
});

test('request witness baseline contains only required object fields', () => {
  // Mutation guard: including optional fields in the baseline would make
  // omission unobservable and confound every one-at-a-time field witness.
  const object = contract.parameters[1].value.variants[1];
  assert.deepEqual(baselineRequestValue(object), { mode: 'one' });
  const witnesses = requestValueWitnesses(object);
  assert.ok(witnesses.some((value) => !Object.hasOwn(value, 'enabled')));
  assert.ok(witnesses.some((value) => Object.hasOwn(value, 'enabled')));
});

test('an operation with only optional parameters has one finite empty baseline', () => {
  // Mutation guard for list/retrieve overloads: trimming trailing omitted
  // parameters must terminate at the empty invocation, not loop on [].at(-1).
  const optionalOnly = {
    parameters: [{ name: 'params', required: false, value: { kind: 'null' } }],
  };
  assert.deepEqual(requestInvocationWitnesses(optionalOnly), [[], [null]]);
  assert.doesNotThrow(() => assertRequestInvocationWitnessCoverage(optionalOnly));
});
