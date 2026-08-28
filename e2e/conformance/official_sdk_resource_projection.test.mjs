import assert from 'node:assert/strict';
import test from 'node:test';

import { officialBetaResourceProjection } from './official_sdk_resource_projection.mjs';

const operation = (id, betas = []) => ({
  id,
  method: 'GET',
  path: '/v1/files',
  transport_query: 'beta=true',
  betas,
});

test('official transport signatures select exactly one Beta or GA projection', () => {
  // Cause/effect graph: C1 every family operation keeps query beta=true; C2 all
  // operations carry one identical endpoint capability; C3 all carry none;
  // C4 signatures are absent, malformed, ambiguous, or mixed. Effects: E1 the
  // historical Beta projection, E2 the post-GA projection, E3 fail closed.
  // Decision table: R1 C1+C2->E1; R2 C1+C3->E2; R3 !C1||C4->E3.
  // Invariant: SDK version numbers and response samples never choose a wire
  // contract; only the generated request signature that reaches the server does.
  assert.deepEqual(
    officialBetaResourceProjection([
      operation('beta.files.list', ['files-api-2025-04-14']),
      operation('beta.files.retrieveMetadata', ['files-api-2025-04-14']),
    ], 'files'),
    { projection: 'beta', capability: 'files-api-2025-04-14' },
    'R1/E1',
  );
  assert.deepEqual(
    officialBetaResourceProjection([
      operation('beta.skills.list'),
      operation('beta.skills.retrieve'),
    ], 'skills'),
    { projection: 'ga' },
    'R2/E2',
  );

  const rejects = [
    [],
    [{ ...operation('beta.files.list'), transport_query: undefined }],
    [operation('beta.files.list', ['one', 'two'])],
    [operation('beta.files.list'), operation('beta.files.retrieveMetadata', ['files-beta'])],
    [operation('beta.files.list', ['one']), operation('beta.files.retrieveMetadata', ['two'])],
  ];
  for (const operations of rejects) {
    assert.throws(() => officialBetaResourceProjection(operations, 'files'), undefined, 'R3/E3');
  }
});
