import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import {
  assignShard,
  expandSuites,
  fileDigest,
  parseShard,
  prebuildFingerprint,
  preparedEnvironment,
  timingWeights,
  validPrebuiltManifest,
  verifyInstalledDependencies,
} from './deterministic_runner.mjs';
import { AWAKEN_BIN_ENV, SCENARIO_HOST_BIN_ENV, WORKER_BIN_ENV } from './cargo_binary.mjs';

const E2E_ROOT_FOR_TEST = path.dirname(fileURLToPath(import.meta.url));

// Cause/effect decision table:
// C1 nested suite, C2 npm pretest hook, C3 cycle; E1 ordered leaf commands,
// E2 pretest executes exactly once before test, E3 cycle fails closed.
// R1 C1+!C3 -> E1; R2 C1+C2+!C3 -> E2; R3 C3 -> E3.
test('expands the package-owned suite graph without a second scenario list', () => {
  const scripts = {
    pretest: 'node prepare.mjs',
    test: 'node base.mjs',
    protocols: 'node protocol.mjs',
    aggregate: 'npm run test && npm run protocols',
  };
  assert.deepEqual(
    expandSuites(scripts, ['aggregate']).map((entry) => entry.command),
    ['node prepare.mjs', 'node base.mjs', 'node protocol.mjs'],
  );
  assert.throws(
    () => expandSuites({ first: 'npm run second', second: 'npm run first' }, ['first']),
    /cyclic npm suite/,
  );
});

test('fails before prebuild when installed E2E SDKs drift from package-lock', () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-e2e-deps-'));
  const installed = path.join(directory, 'node_modules', '@ai-sdk', 'react');
  fs.mkdirSync(installed, { recursive: true });
  const packageDocument = { dependencies: { '@ai-sdk/react': '^4.0.54' } };
  const lockDocument = {
    packages: { 'node_modules/@ai-sdk/react': { version: '4.0.54' } },
  };
  try {
    fs.writeFileSync(path.join(installed, 'package.json'), JSON.stringify({ version: '2.0.212' }));
    assert.throws(
      () => verifyInstalledDependencies(packageDocument, lockDocument, directory),
      /installed=2\.0\.212, locked=4\.0\.54.*npm --prefix e2e ci/,
    );
    fs.writeFileSync(path.join(installed, 'package.json'), JSON.stringify({ version: '4.0.54' }));
    assert.doesNotThrow(() => verifyInstalledDependencies(packageDocument, lockDocument, directory));
    const transitive = path.join(installed, 'node_modules', 'transitive-sdk');
    fs.mkdirSync(transitive, { recursive: true });
    lockDocument.packages['node_modules/@ai-sdk/react/node_modules/transitive-sdk'] = {
      version: '1.0.0',
    };
    fs.writeFileSync(path.join(transitive, 'package.json'), JSON.stringify({ version: '1.0.0' }));
    assert.doesNotThrow(() => verifyInstalledDependencies(packageDocument, lockDocument, directory));
    fs.writeFileSync(path.join(transitive, 'package.json'), JSON.stringify({ version: '2.0.0' }));
    assert.throws(
      () => verifyInstalledDependencies(packageDocument, lockDocument, directory),
      /transitive-sdk: installed=2\.0\.0, locked=1\.0\.0/,
    );
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

// Cause/effect decision table:
// C1 valid one-based shard, C2 invalid bounds, C3 historical duration present;
// E1 every command belongs to exactly one shard, E2 invalid input is rejected,
// E3 the longest commands are balanced rather than colocated.
// R1 C1+!C3 -> E1; R2 C2 -> E2; R3 C1+C3 -> E1+E3.
test('partitions commands exactly once and balances historical durations', () => {
  assert.deepEqual(parseShard('2/3'), { index: 1, total: 3 });
  assert.throws(() => parseShard('0/3'), /invalid shard/);
  const commands = ['a', 'b', 'c', 'd'].map((command) => ({ command, owner: 'suite' }));
  const weights = timingWeights({
    commands: [
      { command: 'a', durationMs: 10 },
      { command: 'b', durationMs: 9 },
      { command: 'c', durationMs: 1 },
      { command: 'd', durationMs: 1 },
    ],
  });
  const first = assignShard(commands, { index: 0, total: 2 }, weights);
  const second = assignShard(commands, { index: 1, total: 2 }, weights);
  assert.deepEqual(new Set([...first, ...second].map((entry) => entry.command)), new Set(['a', 'b', 'c', 'd']));
  assert.equal(first.length + second.length, commands.length);
  assert.notEqual(first.some((entry) => entry.command === 'a'), first.some((entry) => entry.command === 'b'));
});

// Cache-key decision table: C1 environment affects generated machine code,
// C2 environment only relocates Cargo state; E1 invalidate reusable binaries,
// E2 keep the cache portable between builder and shard jobs.
// R1 C1 -> E1; R2 C2 -> E2.
test('fingerprints build inputs but ignores Cargo storage locations', () => {
  const baseline = prebuildFingerprint({});
  assert.equal(prebuildFingerprint({ CARGO_TARGET_DIR: '/different/target' }), baseline);
  assert.notEqual(prebuildFingerprint({ RUSTFLAGS: '-C target-cpu=native' }), baseline);
  assert.notEqual(prebuildFingerprint({ CARGO_PROFILE_RELEASE_LTO: 'true' }), baseline);
});

// Artifact consistency boundary: C1 all three immutable artifacts exist, C2 only
// one exists, C3 source/build fingerprint matches, C4 binary digests match.
// R1 C1+C3+C4 -> reuse exact set without Cargo; R2 C2 -> fail before a shard
// can combine builds; R3 C1+(!C3|!C4) -> reject reuse and rebuild the set.
test('reuses only a complete explicit prebuilt artifact set', () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-runner-test-'));
  const suffix = process.platform === 'win32' ? '.exe' : '';
  try {
    const awaken = path.join(directory, `awaken${suffix}`);
    const scenarioHost = path.join(directory, `awaken-scenario-host${suffix}`);
    const worker = path.join(directory, `awaken-worker${suffix}`);
    fs.writeFileSync(awaken, 'awaken');
    assert.throws(() => preparedEnvironment({}, directory), /incomplete E2E prebuilt directory/);
    fs.writeFileSync(scenarioHost, 'scenario');
    assert.throws(() => preparedEnvironment({}, directory), /incomplete E2E prebuilt directory/);
    fs.writeFileSync(worker, 'worker');
    const fingerprint = prebuildFingerprint({});
    const dependencyLockDigest = fileDigest(path.join(E2E_ROOT_FOR_TEST, 'package-lock.json'));
    const manifest = {
      version: 2,
      fingerprint,
      dependencyLockDigest,
      binaries: {
        awaken: fileDigest(awaken),
        scenarioHost: fileDigest(scenarioHost),
        worker: fileDigest(worker),
      },
    };
    fs.writeFileSync(path.join(directory, 'manifest.json'), JSON.stringify(manifest));
    const environment = preparedEnvironment({}, directory);
    assert.equal(environment[AWAKEN_BIN_ENV], awaken);
    assert.equal(environment[SCENARIO_HOST_BIN_ENV], scenarioHost);
    assert.equal(environment[WORKER_BIN_ENV], worker);
    fs.writeFileSync(awaken, 'corrupt');
    assert.equal(
      validPrebuiltManifest(
        manifest,
        fingerprint,
        dependencyLockDigest,
        awaken,
        scenarioHost,
        worker,
      ),
      false,
    );
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});
