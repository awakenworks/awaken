import assert from 'node:assert/strict';
import { createHmac, randomBytes } from 'node:crypto';

const RESPONSE_FINGERPRINT_ENV = 'AWAKEN_MANAGED_SDK_RESPONSE_FINGERPRINT_KEY';
const configuredFingerprintKey = process.env[RESPONSE_FINGERPRINT_ENV];
if (configuredFingerprintKey !== undefined) {
  assert.match(
    configuredFingerprintKey,
    /^[0-9a-f]{64}$/u,
    `${RESPONSE_FINGERPRINT_ENV} must be one 256-bit lowercase hexadecimal key`,
  );
}
export const MANAGED_SDK_RESPONSE_FINGERPRINT_KEY =
  configuredFingerprintKey ?? randomBytes(32).toString('hex');

function primitiveShape(kind, value) {
  // Receipts cross a process boundary, so an in-memory WeakMap cannot carry
  // literal evidence back to the owner runner. A keyed digest retains exact
  // discriminator evidence without serializing credentials, metadata, user
  // text, or another response value. The key lives only for this test run.
  return Object.freeze({
    kind,
    fingerprint: createHmac('sha256', MANAGED_SDK_RESPONSE_FINGERPRINT_KEY)
      .update(JSON.stringify(value))
      .digest('hex'),
  });
}

function pathMatches(template, actual) {
  const expectedSegments = template.split('/');
  const actualSegments = actual.split('/');
  return expectedSegments.length === actualSegments.length
    && expectedSegments.every((segment, index) => (
      segment === '{}' ? actualSegments[index].length > 0 : segment === actualSegments[index]
    ));
}

function normalizedBetas(values) {
  return [...new Set(values)].sort();
}

function stableShape(value) {
  if (value === null) return Object.freeze({ kind: 'null' });
  if (Array.isArray(value)) {
    const unique = new Map(value.map((item) => {
      const shape = stableShape(item);
      return [JSON.stringify(shape), shape];
    }));
    return Object.freeze({ kind: 'array', items: Object.freeze([...unique.values()]) });
  }
  if (typeof value === 'object') {
    return Object.freeze({
      kind: 'object',
      fields: Object.freeze(Object.fromEntries(
        Object.entries(value)
          .sort(([left], [right]) => left.localeCompare(right))
          .map(([name, nested]) => [name, stableShape(nested)]),
      )),
    });
  }
  if (typeof value === 'string') return primitiveShape('string', value);
  if (typeof value === 'number') return primitiveShape('number', value);
  if (typeof value === 'boolean') return primitiveShape('boolean', value);
  throw new Error(`unsupported JSON response value ${typeof value}`);
}

async function responseShape(response) {
  if (response.status === 204 || response.status === 205) return Object.freeze({ kind: 'empty' });
  const contentType = response.headers.get('content-type')?.split(';', 1)[0].trim().toLowerCase();
  if (contentType === 'text/event-stream') return Object.freeze({ kind: 'stream' });
  if (contentType === 'application/json' || contentType?.endsWith('+json')) {
    const text = await response.clone().text();
    return text.length === 0 ? Object.freeze({ kind: 'empty' }) : stableShape(JSON.parse(text));
  }
  return Object.freeze({ kind: 'binary' });
}

export async function managedSdkReceipt(input, init, response) {
  const request = input instanceof Request && init === undefined
    ? input
    : new Request(input, init);
  const url = new URL(request.url);
  return Object.freeze({
    method: request.method,
    path: url.pathname,
    beta: url.searchParams.get('beta'),
    betas: (request.headers.get('anthropic-beta') ?? '')
      .split(',')
      .map((value) => value.trim())
      .filter(Boolean),
    sdk: request.headers.get('x-stainless-lang') === 'js',
    sdkVersion: request.headers.get('x-stainless-package-version'),
    status: response.status,
    responseShape: await responseShape(response),
  });
}

export function recordingFetch(fetchImplementation, record) {
  return async (input, init) => {
    const request = new Request(input, init);
    const response = await fetchImplementation(input, init);
    await record(await managedSdkReceipt(request, undefined, response));
    return response;
  };
}

function shapeFailures(actual, expected, path) {
  if (expected.kind === 'open-json') return [];
  if (expected.kind === 'union') {
    const attempts = expected.variants.map((variant) => shapeFailures(actual, variant, path));
    return attempts.some((failures) => failures.length === 0)
      ? []
      : [`${path}: no official union variant matched (${attempts.map((value) => value.join('; ')).join(' | ')})`];
  }
  if (expected.kind === 'literal') {
    if (actual?.kind !== expected.primitive) {
      return [`${path}: expected literal ${JSON.stringify(expected.value)}, received ${actual?.kind ?? 'absent'}`];
    }
    const expectedFingerprint = primitiveShape(expected.primitive, expected.value).fingerprint;
    return actual.fingerprint === expectedFingerprint
      ? []
      : [`${path}: expected literal ${JSON.stringify(expected.value)}`];
  }
  if (expected.kind === 'never') return [`${path}: official response type is never`];
  if (actual?.kind !== expected.kind) {
    return [`${path}: expected ${expected.kind}, received ${actual?.kind ?? 'absent'}`];
  }
  if (expected.kind === 'array') {
    return actual.items.flatMap((item, index) => shapeFailures(item, expected.item, `${path}[]#${index}`));
  }
  if (expected.kind !== 'object') return [];

  const failures = [];
  const actualNames = Object.keys(actual.fields);
  for (const [name, property] of Object.entries(expected.properties)) {
    if (!(name in actual.fields)) {
      if (property.required) failures.push(`${path}.${name}: required field is absent`);
      continue;
    }
    failures.push(...shapeFailures(actual.fields[name], property.value, `${path}.${name}`));
  }
  for (const name of actualNames.filter((candidate) => !(candidate in expected.properties))) {
    if (expected.additional === false) failures.push(`${path}.${name}: field is not in the official type`);
    else failures.push(...shapeFailures(actual.fields[name], expected.additional, `${path}.${name}`));
  }
  return failures;
}

export function assertResponseContract(actual, contract, label) {
  if (contract.kind !== 'json') {
    assert.equal(actual?.kind, contract.kind, `${label}: official ${contract.kind} response`);
    return;
  }
  assert.deepEqual(shapeFailures(actual, contract.schema, '$'), [], `${label}: official response shape`);
}

function shapeAtPath(shape, path) {
  return path.reduce(
    (current, name) => (current?.kind === 'object' ? current.fields[name] : undefined),
    shape,
  );
}

function assertAggregateResponseEvidence(receipts, contract, label) {
  for (const path of contract.evidence?.nonEmptyArrays ?? []) {
    assert.ok(
      receipts.some(({ responseShape: shape }) => {
        const observed = shapeAtPath(shape, path);
        return observed?.kind === 'array' && observed.items.length > 0;
      }),
      `${label}: official response item shape at $.${path.join('.')} was never observed`,
    );
  }
}

export function receiptMatchesOperation(receipt, operation, expectedSdkVersion) {
  const actualBetas = new Set(receipt.betas);
  return receipt.sdk === true
    && receipt.sdkVersion === expectedSdkVersion
    && Number.isInteger(receipt.status)
    && receipt.status >= 200
    && receipt.status < 300
    && receipt.method === operation.method
    && pathMatches(operation.route, receipt.path)
    && receipt.beta === (operation.transportQuery === 'beta=true' ? 'true' : null)
    && normalizedBetas(operation.betas).every((beta) => actualBetas.has(beta))
    && normalizedBetas(operation.forbiddenBetas ?? []).every((beta) => !actualBetas.has(beta));
}

function routeSpecificity(route) {
  return route.split('/').reduce(
    (score, segment) => score + (segment === '{}' ? 0 : segment.length + 1),
    0,
  );
}

function attributedOperation(receipt, manifest, expectedSdkVersion) {
  const matches = manifest.filter((operation) => operation.method && receiptMatchesOperation(
    receipt,
    operation,
    expectedSdkVersion,
  ));
  if (matches.length === 0) return undefined;
  const specificity = Math.max(...matches.map(({ route }) => routeSpecificity(route)));
  const mostSpecific = matches.filter(({ route }) => routeSpecificity(route) === specificity);
  assert.equal(
    mostSpecific.length,
    1,
    `successful receipt ambiguously matches ${mostSpecific.map(({ sdkMethod }) => sdkMethod).join(', ')}`,
  );
  return mostSpecific[0];
}

export function assertOwnerOperationReceipts(manifest, owner, receipts, expectedSdkVersion) {
  assert.match(
    expectedSdkVersion ?? '',
    /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u,
    'operation ownership requires one exact SDK version',
  );
  const expected = manifest.filter((entry) => entry.owner === owner && entry.method);
  const missing = expected.filter(
    (operation) => !receipts.some(
      (receipt) => attributedOperation(receipt, manifest, expectedSdkVersion) === operation,
    ),
  );
  assert.deepEqual(
    missing.map(({ sdkMethod, method, route }) => `${sdkMethod} ${method} ${route}`),
    [],
    `${owner}: operations without one successful official SDK ${expectedSdkVersion} response`,
  );
  for (const operation of expected) {
    assert.ok(operation.responseContract, `${operation.sdkMethod}: response contract is absent`);
    const wireContract = operation.wireResponseContract ?? operation.responseContract;
    assert.equal(
      operation.responseContract.kind,
      wireContract.kind,
      `${operation.sdkMethod}: selected SDK and canonical wire media disagree`,
    );
    const matching = receipts.filter(
      (receipt) => attributedOperation(receipt, manifest, expectedSdkVersion) === operation,
    );
    for (const receipt of matching) {
      assertResponseContract(receipt.responseShape, wireContract, operation.sdkMethod);
    }
    assertAggregateResponseEvidence(matching, wireContract, operation.sdkMethod);
  }
  return expected.length;
}
