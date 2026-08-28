import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  extractOperations,
  extractOperationsFromPackageRoot,
} from '../src/extract-operations.mjs';
import { resolveSdkPackage } from '../src/package-source.mjs';

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
    transport_query: 'beta=true',
    betas: ['managed-agents-2026-04-01'],
  });
  assert.equal(byId.get('beta.sessions.list')?.method, 'GET', 'C2/E1');
  assert.equal(byId.get('beta.environments.work.list')?.method, 'GET', 'C3/E1');
  assert.equal(byId.get('beta.files.list')?.transport_query, 'beta=true', 'Beta transport selector');
  assert.deepEqual(byId.get('files.upload'), {
    id: 'files.upload',
    method: 'POST',
    path: '/v1/files',
    betas: [],
  }, 'GA Files is a first-class SDK surface');
  assert.equal(byId.get('files.list')?.transport_query, undefined, 'GA Files has no Beta selector');
  assert.deepEqual(byId.get('files.list')?.betas, [], 'GA Files has no Beta capability');
  assert.equal(byId.get('models.list')?.path, '/v1/models', 'GA Models');
  assert.equal(byId.get('skills.versions.create')?.method, 'POST', 'GA Skills');
  assert.ok(!operations.some(({ id }) => id.startsWith('messages.')), 'ordinary GA Messages stays out of scope');
  assert.ok(!operations.some(({ id }) => id.startsWith('beta.organization.users')), 'E2');
  assert.equal(new Set(operations.map(({ id }) => id)).size, operations.length);
});

test('an explicitly provisioned SDK root has the same authoritative operation inventory', () => {
  // Cause/effect graph: C1 module resolution and C2 an explicit, already
  // provisioned package root identify the same official package. Effect: E1
  // operation/version evidence is identical. Rule R1 C1+C2->E1. Invalid roots
  // fail before source extraction, so candidate canaries cannot scan an
  // arbitrary directory or silently fall back to the current dependency.
  const fromModule = extractOperations('@anthropic-ai/sdk-current', scope);
  const sdk = resolveSdkPackage('@anthropic-ai/sdk-current');
  assert.deepEqual(
    extractOperationsFromPackageRoot(sdk.root, scope, fromModule.module),
    fromModule,
    'R1/E1',
  );
  assert.throws(
    () => extractOperationsFromPackageRoot(packageRoot, scope),
    /is not an @anthropic-ai\/sdk package root/u,
  );
});
