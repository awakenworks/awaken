import assert from 'node:assert/strict';
import path from 'node:path';
import { test } from 'node:test';
import fs from 'node:fs';
import os from 'node:os';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
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
// Platform rule: only the derived default is native-path normalized; an
// explicit caller path remains byte-for-byte unchanged on Windows/macOS/Linux.
test('keeps every E2E Cargo build inside its invoking checkout', () => {
  const inherited = { PATH: '/bin' };
  assert.deepEqual(checkoutLocalCargoEnvironment('/work/a', inherited), {
    PATH: '/bin',
    CARGO_TARGET_DIR: path.resolve('/work/a', 'target'),
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
// R4 final artifact differs from test/intermediate layout and may contain
// spaces, Unicode, or Windows separators/.exe -> preserve Cargo's exact path;
// neither host normalization nor sibling inference is an artifact authority.
test('selects only the exact Cargo executable artifact', () => {
  const output = [
    'not json',
    JSON.stringify({ reason: 'compiler-message', target: { name: 'awaken', kind: ['bin'] }, executable: '/tmp/not-artifact' }),
    JSON.stringify({ reason: 'compiler-artifact', target: { name: 'other', kind: ['bin'] }, executable: '/tmp/other' }),
    JSON.stringify({ reason: 'compiler-artifact', target: { name: 'awaken', kind: ['example'] }, executable: '/tmp/example' }),
    JSON.stringify({ reason: 'compiler-artifact', target: { name: 'awaken', kind: ['bin'] }, executable: '/tmp/awaken' }),
  ].join('\n');
  assert.equal(parseCargoExecutable(output, 'awaken', 'bin'), '/tmp/awaken');
  assert.throws(() => parseCargoExecutable(output, 'missing', 'bin'), /no bin artifact/);
  assert.equal(requirePrebuiltExecutable('BIN', { BIN: process.execPath }), process.execPath);
  for (const executable of [
    '/separate final/工作目录/debug/awaken-sandbox',
    'C:\\separate final\\工作目录\\debug\\awaken-sandbox.exe',
  ]) {
    const artifact = JSON.stringify({
      reason: 'compiler-artifact',
      target: { name: 'awaken-sandbox', kind: ['bin'] },
      executable,
    });
    assert.equal(parseCargoExecutable(`${output}\n${artifact}`, 'awaken-sandbox', 'bin'), executable, 'R4');
  }
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

// Process-level decision table: S1 exact artifact + unrelated records => one
// build and exact stdout path; S2 nonzero Cargo, even with a matching artifact
// => failure + rendered diagnostics; S3 successful Cargo without exact artifact
// => failure; S4 unknown CLI option => reject before Cargo. Cross S1 with the
// actual K3d shell adapter to prove forwarding of feature/toolchain/target inputs.
// These tests execute a fake Cargo process, never a second production resolver.
test('shell and JS consumers share one fail-closed Cargo build', { skip: process.platform === 'win32' }, () => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'cargo-resolver-'));
  try {
    const resolver = fileURLToPath(new URL('./cargo_binary.mjs', import.meta.url));
    const harness = fileURLToPath(new URL('./k3d/harness.sh', import.meta.url));
    const calls = path.join(temp, 'calls');
    fs.writeFileSync(path.join(temp, 'cargo'), `#!/usr/bin/env node
const fs = require('node:fs');
fs.appendFileSync(process.env.CALLS, JSON.stringify({args:process.argv.slice(2), target:process.env.CARGO_TARGET_DIR, toolchain:process.env.RUSTUP_TOOLCHAIN})+'\\n');
if (process.env.MODE !== 'missing') console.log(JSON.stringify({reason:'compiler-artifact', target:{name:'server',kind:['bin']}, executable:'/artifact with spaces/server'}));
if (process.env.MODE === 'failed') {
  console.log(JSON.stringify({reason:'compiler-message', message:{rendered:'fixture compilation failed'}}));
  process.exit(7);
}
`, { mode: 0o755 });
    for (const mode of ['success', 'failed', 'missing', 'invalid', 'k3d']) {
      fs.writeFileSync(calls, '');
      const env = { ...process.env, PATH: `${temp}:${process.env.PATH}`, CALLS: calls, MODE: mode, CARGO_TARGET_DIR: path.join(temp, 'target'), RUSTUP_TOOLCHAIN: 'fixture-toolchain' };
      const options = { cwd: temp, env, encoding: 'utf8' };
      const result = mode === 'k3d'
        ? spawnSync('bash', ['-c', 'source "$1"; resolve_cargo_executable app server --features container-k8s,nats --no-default-features', 'test', harness], options)
        : spawnSync(process.execPath, [resolver, 'app', 'server', ...(mode === 'invalid' ? ['--unknown'] : ['--features', 'container-k8s,nats', '--no-default-features'])], options);
      const builds = fs.readFileSync(calls, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse);
      assert.equal(builds.length, mode === 'invalid' ? 0 : 1, mode);
      if (builds.length) {
        assert.deepEqual(builds[0], {
          args: cargoArguments({ packageName: 'app', targetName: 'server', features: ['container-k8s,nats'], noDefaultFeatures: true }),
          target: env.CARGO_TARGET_DIR,
          toolchain: 'fixture-toolchain',
        }, mode);
      }
      if (mode === 'success' || mode === 'k3d') {
        assert.equal(result.status, 0, result.stderr);
        assert.equal(result.stdout, '/artifact with spaces/server\n');
      } else {
        assert.equal(result.status, 1, mode);
        assert.equal(result.stdout, '', mode);
        assert.match(result.stderr, mode === 'failed' ? /fixture compilation failed/ : mode === 'missing' ? /no bin artifact/ : /Unknown option/);
      }
    }
  } finally {
    fs.rmSync(temp, { recursive: true, force: true });
  }
});
