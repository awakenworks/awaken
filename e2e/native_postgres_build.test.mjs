import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { prepareNativePostgresBundle } from './native_postgres_build.mjs';
import { scenarioHandCompanionPath } from './cargo_binary.mjs';

function fixture(t) {
  const root = fs.mkdtempSync(path.join(tmpdir(), 'native-bundle-law-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const calls = [];
  const write = (name) => {
    fs.mkdirSync(path.dirname(name), { recursive: true });
    fs.writeFileSync(name, path.basename(name));
    return name;
  };
  return { calls, options: {
    output: path.join(root, 'prepared'), sourceDigest: 'a'.repeat(64), platform: 'win32',
    inherited: { PATH: 'owned-tools', AWAKEN_E2E_AWAKEN_BIN: 'foreign-production',
      AWAKEN_E2E_SCENARIO_HOST_BIN: 'foreign-scenario', AWAKEN_E2E_WORKER_BIN: 'foreign-worker',
      ANTHROPIC_API_KEY: 'foreign-provider' }, // awaken-allow(secret): inherited-environment fixture, not a credential
    fetchLocked: (...args) => { calls.push(['fetch', ...args]); },
    buildProduction: (options) => { calls.push(['production', options]); return write(options.snapshotDestination); },
    buildScenario: (options) => {
      calls.push(['scenario', options]);
      const scenarioHost = write(path.join(options.snapshotDirectory, 'awaken-scenario-host.exe'));
      return { scenarioHost, handCompanion: write(scenarioHandCompanionPath(scenarioHost)) };
    },
  } };
}

test('native preparation binds three actual role paths and hashes without ambient prebuilds', (t) => {
  // Production returns a string; scenario owns the shared hand. A receipt must
  // hash that actual hand after locked fetch and exclude every ambient prebuild.
  // Every subprocess boundary below is substituted; this test never uses Cargo.
  const { calls, options } = fixture(t);
  const original = { ...options.inherited };
  const record = prepareNativePostgresBundle(options);
  assert.deepEqual(calls.map(([name]) => name), ['fetch', 'production', 'scenario']);
  assert.deepEqual(calls[0].slice(1, 3), ['cargo', ['fetch', '--locked', '--target', 'x86_64-pc-windows-msvc']]);
  for (const environment of [calls[0][3].env, calls[1][1].environment, calls[2][1].environment]) {
    assert.deepEqual(environment, { PATH: 'owned-tools' });
  }
  assert.deepEqual(options.inherited, original);
  assert.equal(calls[1][1].packageName, 'awaken-cli');
  assert.equal(calls[1][1].targetName, 'awaken');
  assert.equal(record.status, 'built');
  assert.equal(record.source_digest, options.sourceDigest);
  assert.deepEqual(Object.keys(record.binaries), ['production', 'scenario', 'hand']);
  assert.equal(record.binaries.hand.path, scenarioHandCompanionPath(record.binaries.production.path));
  assert.equal(new Set(Object.values(record.binaries).map((row) => row.path)).size, 3);
  for (const row of Object.values(record.binaries)) {
    assert.equal(row.sha256, createHash('sha256').update(fs.readFileSync(row.path)).digest('hex'));
  }
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(options.output, 'build.json'), 'utf8')), record);
  assert.equal(fs.existsSync(path.join(options.output, 'build.pending')), false);
});

test('native preparation refuses invalid inputs and existing output before dispatch', (t) => {
  const { options, calls } = fixture(t);
  for (const changed of [{ platform: 'linux' }, { output: 'relative' }, { sourceDigest: 'bad' }]) {
    assert.throws(() => prepareNativePostgresBundle({ ...options, ...changed }), /native preparation requires/);
  }
  assert.equal(fs.existsSync(options.output), false);
  fs.mkdirSync(options.output);
  const sentinel = path.join(options.output, 'foreign');
  fs.writeFileSync(sentinel, 'preserve');
  assert.throws(() => prepareNativePostgresBundle(options), /EEXIST/);
  assert.equal(fs.readFileSync(sentinel, 'utf8'), 'preserve');
  assert.deepEqual(calls, []);
});

test('native preparation never certifies a failed or incomplete execution bundle', (t) => {
  for (const fault of ['fetch', 'production', 'scenario', 'wrong-companion', 'missing-companion']) {
    const { options, calls } = fixture(t);
    const cause = new Error(`controlled ${fault}`);
    if (fault === 'fetch') options.fetchLocked = () => { throw cause; };
    if (fault === 'production') options.buildProduction = () => { throw cause; };
    if (fault === 'scenario') options.buildScenario = () => { throw cause; };
    if (fault.endsWith('companion')) {
      const build = options.buildScenario;
      options.buildScenario = (args) => {
        const result = build(args);
        if (fault === 'wrong-companion') result.handCompanion = path.join(options.output, 'foreign.exe');
        else fs.unlinkSync(result.handCompanion);
        return result;
      };
    }
    assert.throws(() => prepareNativePostgresBundle(options), (error) =>
      fault.endsWith('companion') ? /colocated|ENOENT/.test(error.message) : error === cause);
    assert.equal(fs.existsSync(path.join(options.output, 'build.json')), false);
    if (fault === 'fetch') assert.deepEqual(calls, []);
    if (fault === 'production') assert.deepEqual(calls.map(([name]) => name), ['fetch']);
  }
});
