import assert from 'node:assert/strict';

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

export function managedSdkReceipt(input, init, response) {
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
  });
}

export function recordingFetch(fetchImplementation, record) {
  return async (input, init) => {
    const request = new Request(input, init);
    const response = await fetchImplementation(input, init);
    record(managedSdkReceipt(request, undefined, response));
    return response;
  };
}

export function receiptMatchesOperation(receipt, operation, expectedSdkVersion) {
  const actualBetas = new Set(receipt.betas);
  return receipt.sdk === true
    && receipt.sdkVersion === expectedSdkVersion
    && Number.isInteger(receipt.status)
    && receipt.status >= 200
    && receipt.status < 500
    && receipt.method === operation.method
    && pathMatches(operation.route, receipt.path)
    && receipt.beta === (operation.transportQuery === 'beta=true' ? 'true' : null)
    && normalizedBetas(operation.betas).every((beta) => actualBetas.has(beta));
}

export function assertOwnerOperationReceipts(manifest, owner, receipts, expectedSdkVersion) {
  assert.match(
    expectedSdkVersion ?? '',
    /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u,
    'operation ownership requires one exact SDK version',
  );
  const expected = manifest.filter((entry) => entry.owner === owner && entry.method);
  for (const operation of expected) {
    assert.ok(
      receipts.some((receipt) => receiptMatchesOperation(
        receipt,
        operation,
        expectedSdkVersion,
      )),
      `${owner}: no official SDK runtime receipt for ${operation.sdkMethod} `
        + `${operation.method} ${operation.route} from SDK ${expectedSdkVersion}`,
    );
  }
  return expected.length;
}
