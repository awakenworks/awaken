import assert from 'node:assert/strict';
import test from 'node:test';

import { projectWireResponseContracts } from '../src/conformance/wire-response-projection.mjs';

const legacyContract = Object.freeze({ kind: 'json', schema: 'legacy' });
const currentContract = Object.freeze({ kind: 'json', schema: 'current' });

function operation(betas) {
  return {
    id: 'beta.files.list',
    method: 'GET',
    path: '/v1/files',
    transport_query: 'beta=true',
    betas,
  };
}

test('wire response projection follows request identity rather than SDK version', () => {
  // Causal decision table:
  // S=same verb/path/query/beta capability; D=different capability.
  // S -> current additive response contract because the server sees identical
  // input bytes and cannot branch on User-Agent. D -> selected historical
  // contract because the capability header names a distinct public projection.
  // Missing contracts fail closed before a behavior receipt can be admitted.
  const selectedResponseContracts = { 'beta.files.list': legacyContract };
  const currentResponseContracts = { 'beta.files.list': currentContract };
  const currentOperations = [operation([])];

  assert.equal(projectWireResponseContracts({
    selectedOperations: [operation([])],
    currentOperations,
    selectedResponseContracts,
    currentResponseContracts,
  })['beta.files.list'], currentContract, 'S');

  assert.equal(projectWireResponseContracts({
    selectedOperations: [operation([
      'managed-agents-2026-04-01',
      'files-api-2025-04-14',
      'managed-agents-2026-04-01',
    ])],
    currentOperations: [operation([
      'files-api-2025-04-14',
      'managed-agents-2026-04-01',
    ])],
    selectedResponseContracts,
    currentResponseContracts,
  })['beta.files.list'], currentContract, 'capability order and duplication do not select a schema');

  assert.equal(projectWireResponseContracts({
    selectedOperations: [operation(['files-api-2025-04-14'])],
    currentOperations,
    selectedResponseContracts,
    currentResponseContracts,
  })['beta.files.list'], legacyContract, 'D');

  assert.throws(() => projectWireResponseContracts({
    selectedOperations: [operation(['files-api-2025-04-14'])],
    currentOperations,
    selectedResponseContracts: {},
    currentResponseContracts,
  }), /selected wire projection has no response contract/u);
});

test('wire response projection fails closed on an unreviewed operation identity', () => {
  // Identity fault model: duplicate selected/current rows can otherwise be
  // silently overwritten by Map/Object construction, while a historical-only
  // operation has no current public projection to compare. Each fault must stop
  // qualification before a more permissive historical schema is selected.
  const selectedResponseContracts = { 'beta.files.list': legacyContract };
  const currentResponseContracts = { 'beta.files.list': currentContract };
  const input = {
    selectedOperations: [operation([])],
    currentOperations: [operation([])],
    selectedResponseContracts,
    currentResponseContracts,
  };

  assert.throws(
    () => projectWireResponseContracts({
      ...input,
      selectedOperations: [operation([]), operation([])],
    }),
    /selected operation beta\.files\.list is duplicated/u,
  );
  assert.throws(
    () => projectWireResponseContracts({
      ...input,
      currentOperations: [operation([]), operation([])],
    }),
    /current operation beta\.files\.list is duplicated/u,
  );
  assert.throws(
    () => projectWireResponseContracts({ ...input, currentOperations: [] }),
    /selected operation is absent from the current oracle/u,
  );
  assert.throws(
    () => projectWireResponseContracts({
      ...input,
      selectedOperations: [{ ...operation([]), betas: [''] }],
    }),
    /beta capabilities must be non-empty strings/u,
  );
});
