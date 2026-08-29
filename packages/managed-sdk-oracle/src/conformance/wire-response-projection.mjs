import assert from 'node:assert/strict';

function capabilitySet(betas, operationID) {
  assert.ok(Array.isArray(betas), `${operationID}: beta capabilities must be an array`);
  assert.ok(
    betas.every((beta) => typeof beta === 'string' && beta.length > 0),
    `${operationID}: beta capabilities must be non-empty strings`,
  );
  return [...new Set(betas)].sort();
}

function requestProjection(operation) {
  return {
    method: operation.method,
    path: operation.path,
    transport_query: operation.transport_query,
    betas: capabilitySet(operation.betas, operation.id),
  };
}

function uniqueByID(operations, label) {
  assert.ok(Array.isArray(operations), `${label} operations must be an array`);
  const byID = new Map();
  for (const operation of operations) {
    assert.equal(typeof operation?.id, 'string', `${label} operation id`);
    assert.ok(!byID.has(operation.id), `${label} operation ${operation.id} is duplicated`);
    byID.set(operation.id, operation);
  }
  return byID;
}

export function projectWireResponseContracts({
  selectedOperations,
  currentOperations,
  selectedResponseContracts,
  currentResponseContracts,
}) {
  const selectedByID = uniqueByID(selectedOperations, 'selected');
  const currentByID = uniqueByID(currentOperations, 'current');
  assert.equal(
    selectedByID.size,
    selectedOperations.length,
    'selected operation identities must be unique',
  );
  return Object.fromEntries(selectedOperations.map((operation) => {
    const current = currentByID.get(operation.id);
    assert.ok(current, `${operation.id}: selected operation is absent from the current oracle`);
    const sameRequestProjection = JSON.stringify(requestProjection(operation))
      === JSON.stringify(requestProjection(current));
    const contracts = sameRequestProjection ? currentResponseContracts : selectedResponseContracts;
    const contract = contracts[operation.id];
    assert.ok(contract, `${operation.id}: selected wire projection has no response contract`);
    return [operation.id, contract];
  }));
}
