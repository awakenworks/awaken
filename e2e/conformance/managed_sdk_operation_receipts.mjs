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

export function receiptMatchesOperation(receipt, operation) {
  const actualBetas = new Set(receipt.betas);
  return receipt.sdk === true
    && Number.isInteger(receipt.status)
    && receipt.status >= 200
    && receipt.status < 500
    && receipt.method === operation.method
    && pathMatches(operation.route, receipt.path)
    && receipt.beta === (operation.transportQuery === 'beta=true' ? 'true' : null)
    && normalizedBetas(operation.betas).every((beta) => actualBetas.has(beta));
}

export function assertOwnerOperationReceipts(manifest, owner, receipts) {
  const expected = manifest.filter((entry) => entry.owner === owner && entry.method);
  for (const operation of expected) {
    assert.ok(
      receipts.some((receipt) => receiptMatchesOperation(receipt, operation)),
      `${owner}: no official SDK runtime receipt for ${operation.sdkMethod} `
        + `${operation.method} ${operation.route}`,
    );
  }
  return expected.length;
}
