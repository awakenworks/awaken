import assert from 'node:assert/strict';
import test from 'node:test';
import {
  ACP_RUNTIME_IDS,
  ACP_RUNTIME_VERSION_SPECS,
  applyAcpRuntimeProfile,
  assertAcpRuntimeVersions,
  parseAcpRuntimes,
  parseVersionTriple,
  projectAcpRuntimeContract,
  resolveAcpRuntimeProfiles,
  versionAtLeast,
} from './acp_runtime_profiles.mjs';

const kimi = {
  key: 'kimi-secret',
  anthropicKey: 'kimi-anthropic-secret',
  anthropicBase: 'https://kimi.example/anthropic',
  anthropicModel: 'kimi-anthropic-model',
  openaiBase: 'https://kimi.example/openai',
  openaiModel: 'kimi-openai-model',
};

const contractRow = (id, executable, args, minimum) => ({
  id,
  discovery: {
    version: { executable, args },
    minimum_version: {
      major: minimum[0],
      minor: minimum[1],
      patch: minimum[2],
    },
  },
});

test('Rust image contract projects ordered runtime identities and version probes', () => {
  // Cause/effect graph: C1 a supported contract has a non-empty ordered set of
  // unique rows; C2 every row has an exact discovery executable/argv and three
  // non-negative version components. Effects: E1 preserve catalog order in the
  // runtime ids; E2 derive each probe command and minimum exactly; E3 expose a
  // recursively immutable projection. Constraint: JavaScript may not supply a
  // fallback id, argv, or minimum. Decision rule R1: C1+C2 => E1+E2+E3 for
  // every row.
  const projected = projectAcpRuntimeContract({
    schema_version: 1,
    runtimes: [
      contractRow('alpha', 'alpha-cli', ['version'], [1, 2, 3]),
      contractRow('beta', 'beta-cli', ['--version'], [4, 5, 6]),
    ],
  });
  assert.deepEqual(projected.ids, ['alpha', 'beta']);
  assert.deepEqual(projected.versionSpecs, {
    alpha: { command: ['alpha-cli', 'version'], minimum: [1, 2, 3] },
    beta: { command: ['beta-cli', '--version'], minimum: [4, 5, 6] },
  });
  assert.ok(Object.isFrozen(projected));
  assert.ok(Object.isFrozen(projected.ids));
  assert.ok(Object.isFrozen(projected.versionSpecs));
  assert.ok(Object.isFrozen(projected.versionSpecs.alpha));
  assert.ok(Object.isFrozen(projected.versionSpecs.alpha.command));
  assert.ok(Object.isFrozen(projected.versionSpecs.alpha.minimum));
});

test('invalid Rust image contract fails closed without a JavaScript catalog fallback', () => {
  // Causes: C1 the root/schema is invalid; C2 rows are absent/non-objects or ids
  // are empty/repeat; C3 discovery/version/minimum facts are missing or invalid.
  // Effect: E1 reject before runtime selection or a process-version probe.
  // Constraint: validation cannot repair the contract from remembered adapter
  // values. Decision rules R2: C1|C2|C3 => E1; only the complete partition is
  // admitted by the projection test above.
  const valid = contractRow('alpha', 'alpha-cli', ['--version'], [1, 2, 3]);
  for (const contract of [
    null,
    { schema_version: 2, runtimes: [valid] },
    { schema_version: 1, runtimes: [] },
    { schema_version: 1, runtimes: [null] },
    { schema_version: 1, runtimes: [contractRow('', 'alpha-cli', [], [1, 2, 3])] },
    { schema_version: 1, runtimes: [valid, valid] },
    { schema_version: 1, runtimes: [{ id: 'alpha' }] },
    { schema_version: 1, runtimes: [{ id: 'alpha', discovery: { version: null } }] },
    { schema_version: 1, runtimes: [contractRow('alpha', '', [], [1, 2, 3])] },
    { schema_version: 1, runtimes: [contractRow('alpha', 'alpha-cli', null, [1, 2, 3])] },
    { schema_version: 1, runtimes: [contractRow('alpha', 'alpha-cli', [1], [1, 2, 3])] },
    { schema_version: 1, runtimes: [{
      id: 'alpha',
      discovery: { version: { executable: 'alpha-cli', args: [] }, minimum_version: null },
    }] },
    { schema_version: 1, runtimes: [contractRow('alpha', 'alpha-cli', [], [1, -1, 3])] },
  ]) {
    assert.throws(
      () => projectAcpRuntimeContract(contract),
      /ACP runtime contract/u,
    );
  }
});

test('all names expands to every production ACP catalog identity', () => {
  // Cause: the Rust-generated catalog projection is loaded. Effect: `all`
  // expands to that complete ordered id list. Constraint: no literal JS list
  // participates. Decision rule R3: generated ids => exact expansion.
  assert.deepEqual(parseAcpRuntimes('all'), ACP_RUNTIME_IDS);
  assert.deepEqual(Object.keys(ACP_RUNTIME_VERSION_SPECS), ACP_RUNTIME_IDS);
});

test('duplicates and unknown runtimes fail before starting a server', () => {
  assert.throws(() => parseAcpRuntimes('claude,claude'), /duplicates/u);
  assert.throws(() => parseAcpRuntimes('kimi'), /unknown ACP runtime/u);
  assert.throws(() => parseAcpRuntimes(' , '), /at least one/u);
});

test('catalog runtime profiles preserve their distinct credential custody', () => {
  // Causes: C1 every generated catalog id is selected; C2 each runtime receives
  // its supported direct-provider or Kimi-compatible credential source. Effects:
  // E1 one profile per catalog id; E2 runtime-specific auth/model facts remain
  // distinct. Constraint: catalog convergence must not merge credential custody.
  // Decision rule R4: C1+C2 => E1+E2 in generated catalog order.
  const profiles = resolveAcpRuntimeProfiles({
    runtimes: [...ACP_RUNTIME_IDS],
    kimi,
    env: {
      OPENAI_API_KEY: 'openai-secret', // awaken-allow: secret -- inert test fixture
      OPENAI_MODEL: 'codex-model',
      GEMINI_API_KEY: 'gemini-secret', // awaken-allow: secret -- inert test fixture
      GEMINI_MODEL: 'gemini-model',
    },
  });
  assert.deepEqual(profiles.map(({ runtime }) => runtime), ACP_RUNTIME_IDS);
  assert.equal(profiles.find(({ runtime }) => runtime === 'codex').auth, 'credential-artifact');
  assert.equal(profiles.find(({ runtime }) => runtime === 'gemini').model, 'gemini-model');
  assert.equal(profiles.find(({ runtime }) => runtime === 'claude').model, 'kimi-anthropic-model');
});

test('missing runtime-specific credential fails instead of falling back', () => {
  assert.throws(
    () => resolveAcpRuntimeProfiles({ runtimes: ['codex'], env: {}, kimi }),
    /Codex requires OPENAI_API_KEY/u,
  );
  assert.throws(
    () => resolveAcpRuntimeProfiles({ runtimes: ['gemini'], env: {}, kimi }),
    /Gemini requires/u,
  );
});

test('applying a profile removes stale credentials from the previous runtime', () => {
  // Causes: C1 every catalog runtime has a resolvable profile; C2 target env
  // contains every key read or emitted by those factories, including the
  // input-only GOOGLE_API_KEY alias; C3 Gemini is selected next. Effects: E1
  // every non-Gemini key and input alias is removed; E2 Gemini's normalized
  // GEMINI_API_KEY plus exact env and AWAKEN routing facts remain. Constraint:
  // the scrub universe comes from the profile factories, never a hand-copied
  // environment list or Kimi object-field names. Decision rule
  // R5=C1+C2+C3=>E1+E2.
  const profiles = resolveAcpRuntimeProfiles({
    runtimes: [...ACP_RUNTIME_IDS],
    kimi,
    env: {
      OPENAI_API_KEY: 'openai-secret', // awaken-allow: secret -- inert test fixture
      OPENAI_MODEL: 'codex-model',
      GEMINI_API_KEY: 'gemini-secret', // awaken-allow: secret -- inert test fixture
      GEMINI_MODEL: 'gemini-model',
    },
  });
  const profile = profiles.find(({ runtime }) => runtime === 'gemini');
  const allProfileKeys = new Set(profiles.flatMap(({ env }) => Object.keys(env)));
  const target = Object.fromEntries([...allProfileKeys].map((key) => [key, 'stale']));
  target.GOOGLE_API_KEY = 'stale-google-alias'; // awaken-allow: secret -- inert test fixture
  applyAcpRuntimeProfile(profile, target);
  for (const key of allProfileKeys) {
    assert.equal(
      target[key],
      Object.hasOwn(profile.env, key) ? profile.env[key] : undefined,
      key,
    );
  }
  assert.equal(target.GOOGLE_API_KEY, undefined);
  assert.equal(target.GEMINI_API_KEY, 'gemini-secret'); // awaken-allow: secret -- inert test fixture
  assert.equal(target.AWAKEN_ACP_CLI, 'gemini');
  assert.equal(target.AWAKEN_MODEL, 'gemini-model');
});

test('decorated runtime versions accept the floor and newer clients', () => {
  assert.deepEqual(parseVersionTriple('codex-cli 0.146.0'), [0, 146, 0]);
  assert.deepEqual(parseVersionTriple('Hermes Agent v0.19.0 (2026.06.19)'), [0, 19, 0]);
  assert.equal(parseVersionTriple('development build'), null);
  assert.equal(versionAtLeast([1, 18, 12], [1, 18, 12]), true);
  assert.equal(versionAtLeast([2, 0, 0], [1, 18, 12]), true);
  assert.equal(versionAtLeast([1, 18, 11], [1, 18, 12]), false);

  const evidence = assertAcpRuntimeVersions({
    runtimes: ['opencode', 'hermes'],
    probe: ({ runtime }) => ({
      status: 0,
      stdout: runtime === 'opencode' ? 'opencode 2.0.0' : 'Hermes Agent v0.19.0',
      stderr: '',
    }),
  });
  assert.deepEqual(evidence, [
    { runtime: 'opencode', version: '2.0.0', minimum: '1.18.12' },
    { runtime: 'hermes', version: '0.19.0', minimum: '0.19.0' },
  ]);
});

test('missing, malformed, and outdated runtime versions fail before real LLM work', () => {
  for (const result of [
    { status: null, stderr: 'not found' },
    { status: 0, stdout: 'development build' },
    { status: 0, stdout: 'opencode 1.18.11' },
  ]) {
    assert.throws(
      () => assertAcpRuntimeVersions({ runtimes: ['opencode'], probe: () => result }),
      /version probe failed|unparseable version|below the compatible minimum/u,
    );
  }
});
