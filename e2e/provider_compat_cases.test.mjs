import assert from 'node:assert/strict';
import test from 'node:test';
import {
  loadProviderCases,
  providerDialects,
  publicProviderCase,
  requiredProviderCaseIds,
  selectProviderCases,
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
  // Cause/effect graph: C1 every built-in API-key provider has its credential
  // and required endpoint; C2 Vertex has its project prerequisite; C3 the
  // custom AnyRouter gateway has both credential and explicit endpoint. E1 the
  // automatic catalog emits every canonical identity exactly once, in order.
  // Constraint K1: with no authored JSON, `autoCases` is the sole catalog owner;
  // prerequisite filtering may omit a row but cannot synthesize another path.
  // Decision rule A1=C1+C2+C3=>E1. Coverage rationale: the all-enabled,
  // dependency-expanded vector detects a missing, duplicate, reordered, or
  // renamed row; adjacent normalization/rejection tests own field-level faults.
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

test('required provider certification selects exact case identities', () => {
  // Cause/effect graph: C1 the strict lane names no/exact/duplicate/invalid
  // case ids; C2 unrelated provider credentials may also be present. E1 no
  // requirement remains opt-in; E2 exact ids are preserved in authored order;
  // E3 duplicate or unsafe ids fail before any provider I/O. Decision rules:
  // R1=!C1=>E1, R2=valid C1+any C2=>E2, R3=duplicate|invalid C1=>E3.
  // Constraints/invariant: the requested ordered identities select only the
  // already-built canonical case catalog; selection never creates another case.
  assert.deepEqual(requiredProviderCaseIds({}), [], 'R1');
  assert.deepEqual(
    requiredProviderCaseIds({ AWAKEN_PROVIDER_MATRIX_REQUIRED_CASE_IDS: 'deepseek,anthropic' }),
    ['deepseek', 'anthropic'],
    'R2',
  );
  const configured = [{ id: 'anthropic' }, { id: 'deepseek' }, { id: 'openai' }];
  assert.equal(selectProviderCases(configured, []), configured, 'R1 all configured');
  assert.deepEqual(
    selectProviderCases(configured, ['deepseek', 'anthropic']).map(({ id }) => id),
    ['deepseek', 'anthropic'],
    'R2 strict ordered subset',
  );
  assert.throws(
    () => requiredProviderCaseIds({ AWAKEN_PROVIDER_MATRIX_REQUIRED_CASE_IDS: 'deepseek,deepseek' }),
    /must be unique/u,
    'R3 duplicate',
  );
  assert.throws(
    () => requiredProviderCaseIds({ AWAKEN_PROVIDER_MATRIX_REQUIRED_CASE_IDS: '../deepseek' }),
    /invalid/u,
    'R3 unsafe',
  );
  assert.throws(
    () => selectProviderCases(configured, ['missing']),
    /not configured/u,
    'R3 missing configured case',
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
