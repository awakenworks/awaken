// Process-level entry for the canonical MemoryMounter copy lifecycle proof.
// The behavior and its cause/effect decision table live beside the Rust
// integration test; this stage gate only executes that single authoritative path.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { REPO_ROOT } from './harness.mjs';

const result = spawnSync(
  'cargo',
  [
    'test',
    '-p',
    'awaken-sandbox-memoryd',
    '--test',
    'copy_lifecycle',
    '--no-default-features',
    '--',
    '--nocapture',
  ],
  { cwd: REPO_ROOT, encoding: 'utf8', stdio: 'inherit' },
);

assert.equal(result.status, 0, `canonical MemoryMounter lifecycle failed: ${result.error ?? ''}`);
console.log('E2E PASS: canonical MemoryMounter copy lifecycle survived replacement.');
