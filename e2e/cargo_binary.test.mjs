import assert from 'node:assert/strict';
import path from 'node:path';
import { test } from 'node:test';
import fs from 'node:fs';
import os from 'node:os';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import {
  cargoArguments,
  cargoExecutable,
  cargoExecutionBundle,
  cargoScenarioHostBundle,
  checkoutLocalCargoEnvironment,
  parseCargoExecutable,
  renderedCargoDiagnostics,
  requirePrebuiltExecutable,
} from './cargo_binary.mjs';
import { withoutAmbientProviderKeys } from './process_environment.mjs';

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

// Snapshot decision table: C1 prebuilt supplied, C2 companion present,
// C3 exact Cargo build required, C4 concurrent feature changes, C5 build fails.
// P1 C1+C2 => independent colocated copies, no Cargo or source mutation;
// P2 C1+!C2 => reject, never fabricate a companion or rebuild silently.
// P3 C3+C4 => each caller executes its own feature bytes, ordinary shared
// target is untouched, intermediate cache remains Cargo-selected.
// P4 C3+C5 => reject, clean only the invocation-owned staging directory.
// P5 copy destination is occupied => reject, retain that occupant and clean
// staging; no shared target or successful snapshot is fabricated on failure.
// Native paths with spaces/Unicode and executable suffixes are intentional;
// these cases run on Windows/macOS/Linux, not a POSIX shell stand-in.
test('prebuilt snapshot bundle is independent and refuses missing companions', () => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'cargo-prebuilt-'));
  try {
    const suffix = process.platform === 'win32' ? '.exe' : '';
    const source = path.join(temp, `awaken-scenario-host${suffix}`);
    const companion = path.join(temp, `awaken-sandbox${suffix}`);
    fs.writeFileSync(source, 'host');
    const options = { cwd: temp, environment: { PATH: '', FIXTURE_HOST: source },
      prebuiltEnvironmentName: 'FIXTURE_HOST', snapshotDirectory: path.join(temp, '快照 space') };
    assert.throws(() => cargoScenarioHostBundle(options), /requires sibling/, 'P2');
    fs.writeFileSync(companion, 'hand');
    const bundle = cargoScenarioHostBundle(options);
    fs.writeFileSync(source, 'replaced');
    fs.writeFileSync(companion, 'replaced');
    assert.equal(fs.readFileSync(bundle.scenarioHost, 'utf8'), 'host', 'P1');
    assert.equal(fs.readFileSync(bundle.handCompanion, 'utf8'), 'hand', 'P1');
    assert.equal(path.dirname(bundle.scenarioHost), path.dirname(bundle.handCompanion));
  } finally { fs.rmSync(temp, { recursive: true, force: true }); }
});

test('real concurrent Cargo builds pin feature bytes before exposing snapshots', async () => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'cargo-concurrent-'));
  const children = [];
  try {
    fs.mkdirSync(path.join(temp, 'src'));
    fs.writeFileSync(path.join(temp, 'Cargo.toml'),
      '[package]\nname="snapshot-fixture"\nversion="0.0.0"\nedition="2021"\n[features]\nsecond=[]\n[workspace]\n');
    fs.writeFileSync(path.join(temp, 'src/main.rs'),
      'fn main() { println!("{}", if cfg!(feature="second") { "second" } else { "first" }); }\n');
    const target = path.join(temp, 'shared target');
    const cache = path.join(temp, 'intermediate cache');
    const environment = { ...withoutAmbientProviderKeys(process.env), RUSTC_WRAPPER: '',
      CARGO_TARGET_DIR: target, CARGO_BUILD_BUILD_DIR: cache };
    const options = { cwd: temp, environment, packageName: 'snapshot-fixture', targetName: 'snapshot-fixture' };
    const shared = cargoExecutable(options);
    const sharedBytes = fs.readFileSync(shared);
    const resolver = new URL('./cargo_binary.mjs', import.meta.url).href;
    const destinations = Array.from({ length: 4 }, (_, i) => path.join(temp, `snapshot ${i} 路径`,
      process.platform === 'win32' ? 'fixture.exe' : 'fixture'));
    const results = await Promise.all(destinations.map((destination, i) => new Promise((resolve) => {
      const child = spawn(process.execPath, ['--input-type=module', '-e',
        `import {cargoExecutable} from ${JSON.stringify(resolver)};
        cargoExecutable(${JSON.stringify({ cwd: temp, packageName: options.packageName,
          targetName: options.targetName, snapshotDestination: destination,
          features: i % 2 ? ['second'] : [] })});`], { env: environment, stdio: ['ignore', 'pipe', 'pipe'] });
      children.push(child);
      let stderr = '';
      child.stderr.on('data', (data) => { stderr += data; });
      child.on('error', (error) => resolve({ error }));
      child.on('close', (code, signal) => resolve({ code, signal, stderr }));
    })));
    for (const [i, result] of results.entries()) {
      assert.equal(result.code, 0, JSON.stringify(result));
      const run = spawnSync(destinations[i], [], { encoding: 'utf8' });
      assert.equal(run.status, 0, run.stderr);
      assert.equal(run.stdout.trim(), i % 2 ? 'second' : 'first', 'P3');
      assert.equal(fs.readdirSync(path.dirname(destinations[i])).length, 1, 'staging cleaned');
    }
    assert.deepEqual(fs.readFileSync(shared), sharedBytes, 'P3: shared final output was not republished');
    assert.ok(fs.existsSync(cache), 'P3: selected intermediate cache reused');
    const failed = path.join(temp, 'failed', 'snapshot');
    assert.throws(() => cargoExecutable({ ...options, features: ['nonexistent'], snapshotDestination: failed }),
      /cargo build failed/, 'P4');
    assert.deepEqual(fs.readdirSync(path.dirname(failed)), [], 'P4: no failed snapshot or abandoned staging');
    assert.deepEqual(fs.readFileSync(shared), sharedBytes, 'P4: unrelated output survives failure');
    fs.mkdirSync(failed);
    fs.writeFileSync(path.join(failed, 'occupant'), 'preserve');
    assert.throws(() => cargoExecutable({ ...options, snapshotDestination: failed }), /cargo build failed/, 'P5');
    assert.deepEqual(fs.readdirSync(path.dirname(failed)), ['snapshot'], 'P5: staging removed');
    assert.equal(fs.readFileSync(path.join(failed, 'occupant'), 'utf8'), 'preserve', 'P5');
  } finally {
    // All child exits are joined above before removing this test's own tree.
    await Promise.all(children.filter((child) => child.exitCode === null && child.signalCode === null)
      .map((child) => new Promise((resolve) => child.once('close', resolve))));
    fs.rmSync(temp, { recursive: true, force: true });
  }
});

// Metadata authority refusal: M1 malformed JSON, M2 missing directory,
// M3 relative directory => all fail before build/staging. No guessed cache
// location or alternate Cargo build may conceal an incompatible toolchain.
test('snapshot builds reject missing or invalid Cargo directory authority', { skip: process.platform === 'win32' }, () => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'cargo-metadata-'));
  try {
    fs.writeFileSync(path.join(temp, 'cargo'), `#!/usr/bin/env node
if (process.argv[2] !== 'metadata') throw new Error('unexpected build after invalid metadata');
console.log(process.env.METADATA);
`, { mode: 0o755 });
    for (const metadata of ['invalid', '{}', '{"build_directory":"relative"}']) {
      assert.throws(() => cargoExecutable({ cwd: temp, packageName: 'fixture', targetName: 'fixture',
        environment: { ...withoutAmbientProviderKeys(process.env), PATH: `${temp}:${process.env.PATH}`, METADATA: metadata },
        snapshotDestination: path.join(temp, 'snapshot', 'fixture') }), /cargo build failed/);
      assert.equal(fs.existsSync(path.join(temp, 'snapshot')), false, metadata);
    }
  } finally { fs.rmSync(temp, { recursive: true, force: true }); }
});

// Bounded formal transition model of the build/copy boundary, attached to its
// executable regression authority. Two callers request different feature bytes;
// Cargo serializes each remove+publish pair in one build-directory lock. Copy
// happens after releasing that lock; cleanup/cancellation may follow publication.
// Explore ALL interleavings, not random schedules. Invariant: every returned
// snapshot contains its requesting caller's bytes. Positive witness: both return.
// TCB: mkdtemp gives disjoint paths, no external actor mutates private outputs,
// Cargo publishes the requested build before exit, copy is independent. This is
// bounded model checking, not a proof of Cargo/filesystem internals or arbitrary
// hostile mutations. Mutant M1 aliases final outputs: must yield a counterexample
// (missing/wrong bytes); M2 omits cleanup isolation: must also be detected.
test('exhaustively checks build/copy/cancel interleavings and rejects shared-output mutants', () => {
  function explore(shared, foreignCleanup = false) {
    const seen = new Set();
    let completed = false;
    let violation = false;
    function visit(state) {
      const key = JSON.stringify(state);
      if (seen.has(key)) return;
      seen.add(key);
      if (state.returned.every(Boolean)) completed = true;
      for (const actor of [0, 1]) {
        const next = structuredClone(state);
        const output = shared ? 0 : actor;
        switch (state.pc[actor]) {
          case 0: // Cargo acquires shared cache lock and removes old final path.
            if (state.lock !== null) continue;
            next.lock = actor; next.files[output] = null; break;
          case 1: // Cargo publishes exact features and releases its lock.
            next.files[output] = actor; next.lock = null; break;
          case 2: { // Cancellation before copy still cleans only this owner.
            const cancelled = structuredClone(state);
            cancelled.pc[actor] = 3;
            visit(cancelled);
            if (state.files[output] !== actor) { violation = true; continue; }
            next.returned[actor] = true; break;
          }
          case 3: // finally after copy or cancellation
            next.files[foreignCleanup ? 1 - output : output] = null; break;
          default: continue;
        }
        next.pc[actor] += 1;
        visit(next);
      }
    }
    visit({ pc: [0, 0], files: [null, null], lock: null, returned: [false, false] });
    return { violation, completed, states: seen.size };
  }
  const correct = explore(false);
  assert.equal(correct.violation, false);
  assert.equal(correct.completed, true, 'non-vacuous two-caller completion');
  assert.ok(correct.states > 20);
  assert.equal(explore(true).violation, true, 'M1 shared outputs must be rejected');
  assert.equal(explore(false, true).violation, true, 'M2 cross-owner cleanup must be rejected');
});

// Production bundle cause/effect table:
// C1 cold source or prebuilt host; C2 companion present/missing; C3 hand feature.
// B3 cold source -> build both exact binaries, hand enabled, colocated output.
// B4 prebuilt+missing -> fail before process startup, never restart-loop a Worker.
// B5 prebuilt+present -> reuse the qualified pair without invoking Cargo.
// Compile tiny real fixture crates to verify command selection and feature bytes;
// actual product E2E owns namespace startup and execution capability validation.
test('production fixtures prepare the complete execution bundle on a cold checkout', () => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'production-bundle-'));
  try {
    fs.writeFileSync(path.join(temp, 'Cargo.toml'), '[workspace]\nmembers=["host","companion"]\nresolver="2"\n');
    for (const [directory, packageName, binary, body] of [
      ['host', 'awaken-cli', 'awaken', 'fn main() { println!("host"); }'],
      ['companion', 'awaken-sandbox', 'awaken-sandbox', 'fn main() { println!("{}", cfg!(feature="hand")); }'],
    ]) {
      fs.mkdirSync(path.join(temp, directory, 'src'), { recursive: true });
      fs.writeFileSync(path.join(temp, directory, 'Cargo.toml'),
        `[package]\nname="${packageName}"\nversion="0.0.0"\nedition="2021"\n[[bin]]\nname="${binary}"\npath="src/main.rs"\n[features]\nhand=[]\n`);
      fs.writeFileSync(path.join(temp, directory, 'src/main.rs'), body);
    }
    const environment = { ...withoutAmbientProviderKeys(process.env), RUSTC_WRAPPER: '',
      CARGO_TARGET_DIR: path.join(temp, 'target'), CARGO_BUILD_BUILD_DIR: path.join(temp, 'cache') };
    const options = { cwd: temp, environment, packageName: 'awaken-cli', targetName: 'awaken' };
    const bundle = cargoExecutionBundle(options);
    assert.equal(path.dirname(bundle.executable), path.dirname(bundle.handCompanion), 'B3');
    assert.equal(spawnSync(bundle.executable, [], { encoding: 'utf8' }).stdout.trim(), 'host');
    assert.equal(spawnSync(bundle.handCompanion, [], { encoding: 'utf8' }).stdout.trim(), 'true');
    const prebuilt = { ...options, environment: { PATH: '', FIXTURE_HOST: bundle.executable }, prebuiltEnvironmentName: 'FIXTURE_HOST' };
    assert.deepEqual(cargoExecutionBundle(prebuilt), bundle, 'B5');
    fs.unlinkSync(bundle.handCompanion);
    assert.throws(() => cargoExecutionBundle(prebuilt), /requires sibling execution companion/, 'B4');
  } finally { fs.rmSync(temp, { recursive: true, force: true }); }
});
