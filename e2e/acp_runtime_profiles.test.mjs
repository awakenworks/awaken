import assert from 'node:assert/strict';
import test from 'node:test';
import {
  ACP_RUNTIME_IDS,
  applyAcpRuntimeProfile,
  assertAcpRuntimeVersions,
  parseAcpRuntimes,
  parseVersionTriple,
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

test('all names expands to every production ACP catalog identity', () => {
  assert.deepEqual(parseAcpRuntimes('all'), ACP_RUNTIME_IDS);
});

test('duplicates and unknown runtimes fail before starting a server', () => {
  assert.throws(() => parseAcpRuntimes('claude,claude'), /duplicates/u);
  assert.throws(() => parseAcpRuntimes('kimi'), /unknown ACP runtime/u);
  assert.throws(() => parseAcpRuntimes(' , '), /at least one/u);
});

test('five runtime profiles preserve their distinct credential custody', () => {
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
  const target = { OPENAI_API_KEY: 'stale', OPENCODE_CONFIG_CONTENT: 'stale' };
  const [profile] = resolveAcpRuntimeProfiles({
    runtimes: ['gemini'],
    env: { GEMINI_API_KEY: 'gemini-secret' }, // awaken-allow: secret -- inert test fixture
  });
  applyAcpRuntimeProfile(profile, target);
  assert.equal(target.OPENAI_API_KEY, undefined);
  assert.equal(target.OPENCODE_CONFIG_CONTENT, undefined);
  assert.equal(target.GEMINI_API_KEY, 'gemini-secret');
  assert.equal(target.AWAKEN_ACP_CLI, 'gemini');
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
