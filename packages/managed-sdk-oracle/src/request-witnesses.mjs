import assert from 'node:assert/strict';

export const MANAGED_UPLOAD_WITNESS = Object.freeze({
  __managed_sdk_upload__: Object.freeze({
    content_base64: Buffer.from('managed-upload-witness').toString('base64'),
    filename: 'managed fixture.txt',
    media_type: 'text/plain',
  }),
});

const OPEN_JSON_WITNESS = Object.freeze({
  managed_fixture: Object.freeze({ enabled: true, nullable: null }),
});

const baselineCache = new WeakMap();
const witnessCache = new WeakMap();
const identityCache = new WeakMap();

function stable(value) {
  if (Array.isArray(value)) return value.map(stable);
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value)
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([key, nested]) => [key, stable(nested)]));
  }
  return value;
}

function identity(value) {
  if (!value || typeof value !== 'object') return JSON.stringify(value);
  const cached = identityCache.get(value);
  if (cached) return cached;
  const serialized = JSON.stringify(stable(value));
  identityCache.set(value, serialized);
  return serialized;
}

function distinct(values) {
  return [...new Map(values.map((value) => [identity(value), value])).values()];
}

export function baselineRequestValue(schema) {
  if (baselineCache.has(schema)) return baselineCache.get(schema);
  let value;
  switch (schema.kind) {
    case 'open-json': value = OPEN_JSON_WITNESS; break;
    case 'upload': value = MANAGED_UPLOAD_WITNESS; break;
    case 'literal': value = schema.value; break;
    case 'string': value = 'managed fixture /?% ü'; break;
    case 'number': value = 1; break;
    case 'boolean': value = true; break;
    case 'null': value = null; break;
    case 'array': value = Object.freeze([]); break;
    case 'object': value = Object.freeze(Object.fromEntries(Object.entries(schema.properties)
      .filter(([, property]) => property.required)
      .map(([name, property]) => [name, baselineRequestValue(property.value)]))); break;
    case 'union': {
      const candidates = schema.variants.filter(({ kind }) => kind !== 'never');
      assert.ok(candidates.length > 0, 'request union contains only never');
      value = baselineRequestValue(
        candidates.find(({ kind }) => kind !== 'null') ?? candidates[0],
      );
      break;
    }
    case 'never': throw new Error('request contract cannot materialize never');
    default: throw new Error(`unsupported request contract kind ${schema.kind}`);
  }
  baselineCache.set(schema, value);
  return value;
}

export function requestValueWitnesses(schema) {
  if (witnessCache.has(schema)) return witnessCache.get(schema);
  let generated;
  switch (schema.kind) {
    case 'never': generated = []; break;
    case 'string': generated = ['', baselineRequestValue(schema)]; break;
    case 'number': generated = [-1, 0, baselineRequestValue(schema)]; break;
    case 'boolean': generated = [true, false]; break;
    case 'union': generated = distinct(schema.variants.flatMap(requestValueWitnesses)); break;
    case 'array': generated = distinct([
      [],
      ...requestValueWitnesses(schema.item).map((item) => [item]),
    ]); break;
    case 'object': {
      const base = baselineRequestValue(schema);
      generated = [base];
      for (const [name, property] of Object.entries(schema.properties)) {
        for (const value of requestValueWitnesses(property.value)) {
          generated.push({ ...base, [name]: value });
        }
      }
      if (schema.additional !== false) {
        for (const value of requestValueWitnesses(schema.additional)) {
          generated.push({ ...base, managed_additional_fixture: value });
        }
      }
      generated = distinct(generated);
      break;
    }
    default: generated = [baselineRequestValue(schema)];
  }
  const frozen = Object.freeze(generated);
  witnessCache.set(schema, frozen);
  return frozen;
}

export function requestInvocationWitnesses(contract) {
  const baseline = contract.parameters.map((parameter) => (
    parameter.required ? baselineRequestValue(parameter.value) : undefined
  ));
  while (baseline.length > 0 && baseline.at(-1) === undefined) baseline.pop();
  const generated = [baseline];
  for (const [index, parameter] of contract.parameters.entries()) {
    for (const value of requestValueWitnesses(parameter.value)) {
      const invocation = [...baseline];
      while (invocation.length <= index) invocation.push(undefined);
      invocation[index] = value;
      generated.push(invocation);
    }
  }
  return distinct(generated);
}

function includes(values, expected) {
  const expectedIdentity = identity(expected);
  return values.some((value) => identity(value) === expectedIdentity);
}

export function assertRequestValueWitnessCoverage(schema) {
  const generated = requestValueWitnesses(schema);
  assert.ok(generated.length > 0 || schema.kind === 'never');
  switch (schema.kind) {
    case 'union':
      for (const variant of schema.variants) {
        assertRequestValueWitnessCoverage(variant);
        for (const value of requestValueWitnesses(variant)) {
          assert.ok(includes(generated, value), 'request union branch is uncovered');
        }
      }
      break;
    case 'array':
      assert.ok(includes(generated, []), 'request empty-array branch is uncovered');
      assertRequestValueWitnessCoverage(schema.item);
      for (const value of requestValueWitnesses(schema.item)) {
        assert.ok(includes(generated, [value]), 'request array item branch is uncovered');
      }
      break;
    case 'object': {
      const base = baselineRequestValue(schema);
      for (const [name, property] of Object.entries(schema.properties)) {
        assert.equal(Object.hasOwn(base, name), property.required, `${name}: omission branch`);
        assertRequestValueWitnessCoverage(property.value);
        for (const value of requestValueWitnesses(property.value)) {
          assert.ok(
            includes(generated, { ...base, [name]: value }),
            `${name}: request property branch is uncovered`,
          );
        }
      }
      if (schema.additional !== false) {
        assertRequestValueWitnessCoverage(schema.additional);
        for (const value of requestValueWitnesses(schema.additional)) {
          assert.ok(
            includes(generated, { ...base, managed_additional_fixture: value }),
            'request map value branch is uncovered',
          );
        }
      }
      break;
    }
    default:
      break;
  }
}

export function assertRequestInvocationWitnessCoverage(contract) {
  const generated = requestInvocationWitnesses(contract);
  const baseline = contract.parameters.map((parameter) => (
    parameter.required ? baselineRequestValue(parameter.value) : undefined
  ));
  while (baseline.length > 0 && baseline.at(-1) === undefined) baseline.pop();
  assert.ok(includes(generated, baseline), 'required-only request baseline is uncovered');
  for (const [index, parameter] of contract.parameters.entries()) {
    assertRequestValueWitnessCoverage(parameter.value);
    if (!parameter.required) {
      assert.ok(
        baseline.length <= index || baseline[index] === undefined,
        `${parameter.name}: optional parameter omission is uncovered`,
      );
    }
    for (const value of requestValueWitnesses(parameter.value)) {
      assert.ok(
        generated.some((invocation) => invocation.length > index
          && identity(invocation[index]) === identity(value)),
        `${parameter.name}: parameter value branch is uncovered`,
      );
    }
  }
}
