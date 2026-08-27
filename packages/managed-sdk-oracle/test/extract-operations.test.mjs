import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { extractOperations } from '../src/extract-operations.mjs';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const scope = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/scope.json')));

test('current oracle discovers ordinary, paginated, and nested managed operations', () => {
  // Cause/effect graph: C1 direct HTTP call, C2 Stainless getAPIList wrapper,
  // C3 nested resource. Effects: E1 all normalize into the same operation
  // inventory; E2 unrelated Beta Admin resources remain outside the scope.
  const { operations } = extractOperations('@anthropic-ai/sdk-current', scope);
  const byId = new Map(operations.map((operation) => [operation.id, operation]));
  assert.deepEqual(byId.get('beta.sessions.create'), {
    id: 'beta.sessions.create',
    method: 'POST',
    path: '/v1/sessions',
    betas: ['managed-agents-2026-04-01'],
  });
  assert.equal(byId.get('beta.sessions.list')?.method, 'GET', 'C2/E1');
  assert.equal(byId.get('beta.environments.work.list')?.method, 'GET', 'C3/E1');
  assert.ok(!operations.some(({ id }) => id.startsWith('beta.organization.users')), 'E2');
  assert.equal(new Set(operations.map(({ id }) => id)).size, operations.length);
});
