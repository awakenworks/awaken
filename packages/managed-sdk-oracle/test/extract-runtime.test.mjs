import assert from 'node:assert/strict';
import fs from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { managedRuntimeFingerprintFromPackageRoot } from '../src/extract-runtime.mjs';
import { resolveSdkPackage } from '../src/package-source.mjs';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const scope = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/scope.json')));

test('Managed runtime fingerprint follows helpers and shared transport but excludes unrelated APIs', () => {
  // Cause/effect graph: C1 supported resource and helper entrypoints import
  // runtime files; C2 those files import shared transport; C3 Client also wires
  // unrelated APIs. Effects: E1 C1+C2 enter one deterministic fingerprint;
  // E2 C3 stays outside it. This catches behavior-only SDK drift without making
  // Messages or Organization APIs part of Awaken's Managed compatibility claim.
  const sdk = resolveSdkPackage('@anthropic-ai/sdk-current');
  const first = managedRuntimeFingerprintFromPackageRoot(sdk.root, scope);
  const second = managedRuntimeFingerprintFromPackageRoot(sdk.root, scope);
  assert.deepEqual(first, second, 'C1+C2/E1 deterministic closure');
  assert.match(first.fingerprint, /^[0-9a-f]{64}$/u);
  const paths = new Set(first.files.map(({ path: runtimePath }) => runtimePath));
  for (const required of [
    'client.mjs',
    'core/middleware.mjs',
    'lib/environments/worker.mjs',
    'lib/sessions/accumulate.mjs',
    'lib/tools/SessionToolRunner.mjs',
    'resources/beta/sessions/events.mjs',
    'tools/agent-toolset/node.mjs',
    'tools/agent-toolset/skills.mjs',
  ]) {
    assert.ok(paths.has(required), `E1 missing ${required}`);
  }
  assert.equal(paths.has('resources/messages/messages.mjs'), false, 'C3/E2 GA Messages');
  assert.equal(
    paths.has('resources/beta/organization/users/users.mjs'),
    false,
    'C3/E2 Beta Organization',
  );
  assert.ok(first.files.every(({ fingerprint }) => /^[0-9a-f]{64}$/u.test(fingerprint)));
  assert.throws(
    () => managedRuntimeFingerprintFromPackageRoot(packageRoot, scope),
    /is not an @anthropic-ai\/sdk package root/u,
  );
});

test('runtime fingerprint changes exactly when the Managed dependency closure changes', () => {
  // Fault-injection table: F1 mutate a transitive shared dependency; F2 mutate
  // a scoped resource; F3 mutate an imported but excluded API. Expected effects:
  // F1/F2 change both file evidence and aggregate fingerprint; F3 changes
  // neither. This proves the candidate gate is sensitive to behavior-only drift
  // while its explicit non-Managed exclusion cannot create noisy false failures.
  const root = fs.mkdtempSync(path.join(tmpdir(), 'awaken-runtime-fingerprint-'));
  const write = (relative, content) => {
    const filename = path.join(root, relative);
    fs.mkdirSync(path.dirname(filename), { recursive: true });
    fs.writeFileSync(filename, content);
  };
  const fixtureScope = {
    beta_resource_roots: ['sessions'],
    ga_resource_roots: [],
    managed_runtime_entrypoints: ['index.mjs'],
  };
  try {
    write('package.json', JSON.stringify({ name: '@anthropic-ai/sdk', version: '1.0.0' }));
    write('index.mjs', "import './client.mjs';\n");
    write(
      'client.mjs',
      "import './internal/shared.mjs';\n"
        + "import './resources/beta/sessions.mjs';\n"
        + "import './resources/messages/messages.mjs';\n",
    );
    write('internal/shared.mjs', 'export const shared = 1;\n');
    write('resources/beta/sessions.mjs', 'export const sessions = 1;\n');
    write('resources/messages/messages.mjs', 'export const messages = 1;\n');
    const baseline = managedRuntimeFingerprintFromPackageRoot(root, fixtureScope);

    write('internal/shared.mjs', 'export const shared = 2;\n');
    const sharedMutation = managedRuntimeFingerprintFromPackageRoot(root, fixtureScope);
    assert.notEqual(sharedMutation.fingerprint, baseline.fingerprint, 'F1');
    assert.equal(sharedMutation.file_count, baseline.file_count, 'F1 changes content, not closure');

    write('resources/beta/sessions.mjs', 'export const sessions = 2;\n');
    const resourceMutation = managedRuntimeFingerprintFromPackageRoot(root, fixtureScope);
    assert.notEqual(resourceMutation.fingerprint, sharedMutation.fingerprint, 'F2');

    write('resources/messages/messages.mjs', 'export const messages = 2;\n');
    assert.deepEqual(
      managedRuntimeFingerprintFromPackageRoot(root, fixtureScope),
      resourceMutation,
      'F3',
    );
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
