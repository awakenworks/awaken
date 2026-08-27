import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { managedTypeFingerprint } from '../src/extract-types.mjs';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const scope = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/scope.json')));

test('managed declaration fingerprint is deterministic and scoped', () => {
  // Cause/effect graph: C1 exact installed SDK declarations and C2 reviewed
  // Managed resource scope produce E1 one deterministic fingerprint. A package
  // upgrade or scoped type change produces E2 a different review artifact;
  // unrelated Beta Admin declarations never enter the hash.
  const first = managedTypeFingerprint('@anthropic-ai/sdk-current', scope);
  const second = managedTypeFingerprint('@anthropic-ai/sdk-current', scope);
  assert.deepEqual(first, second, 'C1+C2/E1');
  assert.match(first.fingerprint, /^[0-9a-f]{64}$/);
  assert.ok(first.file_count > scope.beta_resource_roots.length);
});
