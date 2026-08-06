import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  parseCargoExecutable,
  renderedCargoDiagnostics,
  requirePrebuiltExecutable,
} from './cargo_binary.mjs';

// Cause/effect design: C1 matching compiler artifact, C2 unrelated/malformed
// records, C3 explicit prebuilt path exists. Effects: E1 exact executable is
// selected, E2 missing artifact fails closed, E3 a valid prebuild bypasses Cargo.
// Decision rules: R1 C1+any C2 -> E1; R2 !C1 -> E2; R3 C3 -> E3.
test('selects only the exact Cargo executable artifact', () => {
  const output = [
    'not json',
    JSON.stringify({ reason: 'compiler-artifact', target: { name: 'other', kind: ['bin'] }, executable: '/tmp/other' }),
    JSON.stringify({ reason: 'compiler-artifact', target: { name: 'awaken', kind: ['example'] }, executable: '/tmp/example' }),
    JSON.stringify({ reason: 'compiler-artifact', target: { name: 'awaken', kind: ['bin'] }, executable: '/tmp/awaken' }),
  ].join('\n');
  assert.equal(parseCargoExecutable(output, 'awaken', 'bin'), '/tmp/awaken');
  assert.throws(() => parseCargoExecutable(output, 'missing', 'bin'), /no bin artifact/);
  assert.equal(requirePrebuiltExecutable('BIN', { BIN: process.execPath }), process.execPath);
});

// Error-guessing coverage: structured diagnostics are authoritative; absent
// structured output falls back to stderr so build failures never become an
// empty or successful result.
test('preserves structured Cargo diagnostics with a stderr fallback', () => {
  assert.equal(renderedCargoDiagnostics({ stdout: '', stderr: 'fallback' }), 'fallback');
  assert.equal(
    renderedCargoDiagnostics({
      stdout: `${JSON.stringify({ reason: 'compiler-message', message: { rendered: 'broken\n' } })}\n`,
      stderr: 'ignored',
    }),
    'broken',
  );
});
