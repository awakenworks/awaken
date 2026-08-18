import assert from 'node:assert/strict';
import test from 'node:test';
import {
  loadProviderCases,
  providerDialects,
  publicProviderCase,
} from './provider_compat_cases.mjs';

const authored = (cases, secrets = {}) => ({
  AWAKEN_PROVIDER_CASES_JSON: JSON.stringify(cases),
  ...secrets,
});

test('authored third-party provider is opaque, normalized, and secret-free in public evidence', () => {
  const [value] = loadProviderCases(authored([{
    id: 'vendor-v2',
    provider_id: 'vendor-v2',
    dialect: 'open_ai_chat',
    base_url: 'https://edge.vendor.example/v9',
    model_id: 'vendor/model',
    secret_env: 'VENDOR_TOKEN',
  }], { VENDOR_TOKEN: 'do-not-print' })); // awaken-allow: secret -- inert non-leak fixture
  assert.equal(value.secret, 'do-not-print');
  assert.equal(value.timeout_secs, 300);
  assert.ok(!JSON.stringify(publicProviderCase(value)).includes('do-not-print'));
});

test('Vertex OAuth helper stays secret-free and keeps endpoint construction server-owned', () => {
  const [value] = loadProviderCases(authored([{
    id: 'vertex-prod',
    provider_id: 'vertex',
    dialect: 'vertex_gemini',
    auth: 'oauth_helper',
    oauth_helper: 'gcloud',
    configuration: { project_id: 'project-a', location: 'global' },
  }]));
  assert.equal(value.base_url, undefined);
  assert.equal(value.secret, undefined);
  assert.deepEqual(value.configuration, { project_id: 'project-a', location: 'global' });
});

test('all built-in wire dialects remain admitted', () => {
  const cases = providerDialects.map((dialect, index) => ({
    id: `case-${index}`,
    provider_id: `provider-${index}`,
    dialect,
    base_url: `https://provider-${index}.example/v1`,
    secret_env: `TOKEN_${index}`,
  }));
  const secrets = Object.fromEntries(cases.map((item) => [item.secret_env, 'secret']));
  assert.equal(loadProviderCases(authored(cases, secrets)).length, providerDialects.length);
});

test('automatic cases cover every installed provider descriptor plus a custom gateway', () => {
  const cases = loadProviderCases({
    ANTHROPIC_API_KEY: 'a',
    OPENAI_API_KEY: 'o',
    DEEPSEEK_API_KEY: 'd',
    KIMI_API_KEY: 'k',
    ANYROUTER_API_KEY: 'r',
    ANYROUTER_BASE_URL: 'https://router.example/v1',
    GEMINI_API_KEY: 'g',
    VERTEX_PROJECT_ID: 'project-a',
  });
  assert.deepEqual(
    cases.map(({ id }) => id),
    ['anthropic', 'openai', 'deepseek', 'kimi-anthropic', 'anyrouter', 'gemini', 'vertex'],
  );
});

for (const [name, cases, secrets, pattern] of [
  ['malformed JSON', '{', {}, /valid JSON/u],
  ['non-array JSON', '{}', {}, /must be an array/u],
  ['missing secret', [{ id: 'x', provider_id: 'x', dialect: 'open_ai_chat', base_url: 'https://x.example', secret_env: 'X_KEY' }], {}, /is not configured/u],
  ['relative URL', [{ id: 'x', provider_id: 'x', dialect: 'open_ai_chat', base_url: '/v1', secret_env: 'X_KEY' }], { X_KEY: 'x' }, /base_url must be absolute/u],
  ['unsupported protocol', [{ id: 'x', provider_id: 'x', dialect: 'open_ai_chat', base_url: 'file:///tmp/x', secret_env: 'X_KEY' }], { X_KEY: 'x' }, /must use HTTP/u],
  ['unknown dialect', [{ id: 'x', provider_id: 'x', dialect: 'mystery', base_url: 'https://x.example', secret_env: 'X_KEY' }], { X_KEY: 'x' }, /unsupported dialect/u],
  ['invented third-party dialect', [{ id: 'x', provider_id: 'x', dialect: 'third-party/v9', base_url: 'https://x.example', secret_env: 'X_KEY' }], { X_KEY: 'x' }, /unsupported dialect/u],
  ['unsafe secret selector', [{ id: 'x', provider_id: 'x', dialect: 'open_ai_chat', base_url: 'https://x.example', secret_env: 'x-key' }], { 'x-key': 'x' }, /secret_env is invalid/u],
  ['OAuth secret ambiguity', [{ id: 'x', provider_id: 'vertex', dialect: 'vertex_gemini', auth: 'oauth_helper', oauth_helper: 'gcloud', secret_env: 'X_KEY', configuration: { project_id: 'p' } }], { X_KEY: 'x' }, /cannot select a secret/u],
  ['OAuth client-owned URL', [{ id: 'x', provider_id: 'vertex', dialect: 'vertex_gemini', auth: 'oauth_helper', oauth_helper: 'gcloud', base_url: 'https://x.example', configuration: { project_id: 'p' } }], {}, /server-owned/u],
]) {
  test(`${name} fails before provider I/O`, () => {
    const env = typeof cases === 'string'
      ? { AWAKEN_PROVIDER_CASES_JSON: cases, ...secrets }
      : authored(cases, secrets);
    assert.throws(() => loadProviderCases(env), pattern);
  });
}

test('duplicate identities fail before provider I/O', () => {
  const one = {
    id: 'duplicate',
    provider_id: 'vendor',
    dialect: 'open_ai_chat',
    base_url: 'https://vendor.example/v1',
    secret_env: 'VENDOR_KEY',
  };
  assert.throws(
    () => loadProviderCases(authored([one, one], { VENDOR_KEY: 'secret' })),
    /ids must be unique/u,
  );
});
