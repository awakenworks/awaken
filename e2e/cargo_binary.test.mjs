import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  cargoArguments,
  checkoutLocalCargoEnvironment,
  parseCargoExecutable,
  renderedCargoDiagnostics,
  requirePrebuiltExecutable,
} from './cargo_binary.mjs';

// Cargo-target isolation cause/effect graph: C1 no target is selected, C2 an
// explicit target is selected, C3 checkout roots differ. Effects: E1 default to
// the invoking checkout's target, E2 preserve the explicit target, E3 unrelated
// worktrees cannot consume same-version stale artifacts. Decision table:
// R1 C1 -> E1; R2 C2 -> E2; R3 C1+C3 -> E1+E3. The returned environment keeps
// all unrelated variables and never mutates the caller-owned input.
test('keeps every E2E Cargo build inside its invoking checkout', () => {
  const inherited = { PATH: '/bin' };
  assert.deepEqual(checkoutLocalCargoEnvironment('/work/a', inherited), {
    PATH: '/bin',
    CARGO_TARGET_DIR: '/work/a/target',
  }, 'R1');
  assert.deepEqual(checkoutLocalCargoEnvironment('/work/a', {
    ...inherited,
    CARGO_TARGET_DIR: '/explicit/target',
  }), {
    PATH: '/bin',
    CARGO_TARGET_DIR: '/explicit/target',
  }, 'R2');
  assert.notEqual(
    checkoutLocalCargoEnvironment('/work/a', inherited).CARGO_TARGET_DIR,
    checkoutLocalCargoEnvironment('/work/b', inherited).CARGO_TARGET_DIR,
    'R3',
  );
  assert.deepEqual(inherited, { PATH: '/bin' }, 'caller environment remains immutable');
});

// Build-argument FMECA and cause/effect decision table:
// C1 target is a binary, C2 target is an example, C3 default features are
// disabled, C4 exact features are requested. Effects: E1 select exactly one
// target flag, E2 preserve the feature floor, E3 never invent an ambient target.
// R1 C1+!C3+!C4 -> --bin; R2 C2+C3+C4 -> --example plus both feature controls.
test('derives every Cargo target and feature argument through one decision path', () => {
  assert.deepEqual(
    cargoArguments({ packageName: 'app', targetName: 'server' }),
    ['build', '--quiet', '--message-format=json', '-p', 'app', '--bin', 'server'],
    'R1',
  );
  assert.deepEqual(
    cargoArguments({
      packageName: 'app',
      targetName: 'fixture',
      targetKind: 'example',
      noDefaultFeatures: true,
      features: ['container', 'memoryd'],
    }),
    [
      'build', '--quiet', '--message-format=json', '-p', 'app', '--example', 'fixture',
      '--no-default-features', '--features', 'container,memoryd',
    ],
    'R2',
  );
});

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
