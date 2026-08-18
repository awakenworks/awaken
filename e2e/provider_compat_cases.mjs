import assert from 'node:assert/strict';

const SAFE_ID = /^[a-z0-9][a-z0-9._-]{0,63}$/u;
const SAFE_ENV = /^[A-Z][A-Z0-9_]{0,127}$/u;
const SAFE_HELPER = /^[a-z0-9][a-z0-9._-]{0,63}$/u;
const DIALECTS = new Set([
  'anthropic_messages',
  'open_ai_chat',
  'open_ai_responses',
  'gemini',
  'vertex_gemini',
]);

const optional = (value) => typeof value === 'string' && value.trim() !== ''
  ? value.trim()
  : undefined;

function autoCases(env) {
  return [
    {
      id: 'anthropic',
      provider_id: 'anthropic',
      dialect: 'anthropic_messages',
      base_url: optional(env.ANTHROPIC_BASE_URL) ?? 'https://api.anthropic.com',
      model_id: optional(env.ANTHROPIC_MODEL),
      secret_env: 'ANTHROPIC_API_KEY',
    },
    {
      id: 'openai',
      provider_id: 'openai',
      dialect: 'open_ai_responses',
      base_url: optional(env.OPENAI_BASE_URL) ?? 'https://api.openai.com/v1',
      model_id: optional(env.OPENAI_MODEL),
      secret_env: 'OPENAI_API_KEY',
    },
    {
      id: 'deepseek',
      provider_id: 'deepseek',
      dialect: 'open_ai_chat',
      base_url: optional(env.DEEPSEEK_BASE_URL) ?? 'https://api.deepseek.com',
      model_id: optional(env.DEEPSEEK_MODEL),
      secret_env: 'DEEPSEEK_API_KEY',
    },
    {
      id: 'kimi-anthropic',
      provider_id: 'kimi',
      dialect: 'anthropic_messages',
      base_url: optional(env.KIMI_ANTHROPIC_BASE_URL)
        ?? optional(env.KIMI_BASE_URL)
        ?? 'https://api.kimi.com/coding/v1',
      model_id: optional(env.KIMI_ANTHROPIC_MODEL)
        ?? optional(env.KIMI_MODEL)
        ?? 'kimi-for-coding',
      secret_env: optional(env.KIMI_ANTHROPIC_API_KEY) ? 'KIMI_ANTHROPIC_API_KEY' : 'KIMI_API_KEY', // awaken-allow: secret -- selector name only
    },
    {
      id: 'anyrouter',
      provider_id: 'anyrouter',
      dialect: optional(env.ANYROUTER_DIALECT) ?? 'open_ai_chat',
      base_url: optional(env.ANYROUTER_BASE_URL),
      model_id: optional(env.ANYROUTER_MODEL),
      secret_env: 'ANYROUTER_API_KEY',
    },
    {
      id: 'gemini',
      provider_id: 'gemini',
      dialect: 'gemini',
      base_url: optional(env.GEMINI_BASE_URL)
        ?? 'https://generativelanguage.googleapis.com/v1beta',
      model_id: optional(env.GEMINI_MODEL),
      secret_env: optional(env.GEMINI_API_KEY) ? 'GEMINI_API_KEY' : 'GOOGLE_API_KEY', // awaken-allow: secret -- selector name only
    },
    {
      id: 'vertex',
      provider_id: 'vertex',
      dialect: 'vertex_gemini',
      model_id: optional(env.VERTEX_MODEL),
      auth: 'oauth_helper',
      oauth_helper: optional(env.VERTEX_OAUTH_HELPER) ?? 'gcloud',
      configuration: {
        project_id: optional(env.VERTEX_PROJECT_ID),
        location: optional(env.VERTEX_LOCATION) ?? 'global',
      },
    },
  ].filter((item) => item.auth === 'oauth_helper'
    ? optional(item.configuration?.project_id)
    : optional(env[item.secret_env]) && optional(item.base_url));
}

function parseAuthoredCases(env) {
  const raw = optional(env.AWAKEN_PROVIDER_CASES_JSON);
  if (!raw) return [];
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch (error) {
    throw new Error(`AWAKEN_PROVIDER_CASES_JSON must be valid JSON: ${error.message}`);
  }
  assert.ok(Array.isArray(parsed), 'AWAKEN_PROVIDER_CASES_JSON must be an array');
  return parsed;
}

function normalizeCase(input, env) {
  assert.ok(input && typeof input === 'object' && !Array.isArray(input), 'provider case must be an object');
  const value = {
    id: optional(input.id),
    provider_id: optional(input.provider_id),
    dialect: optional(input.dialect),
    base_url: optional(input.base_url),
    model_id: optional(input.model_id),
    auth: optional(input.auth) ?? 'api_key',
    secret_env: optional(input.secret_env),
    oauth_helper: optional(input.oauth_helper),
    configuration: input.configuration,
    timeout_secs: input.timeout_secs === undefined ? 300 : Number(input.timeout_secs),
  };
  assert.match(value.id ?? '', SAFE_ID, 'provider case id must be a stable safe identifier');
  assert.match(value.provider_id ?? '', SAFE_ID, `${value.id}: provider_id is invalid`);
  assert.ok(
    DIALECTS.has(value.dialect),
    `${value.id}: unsupported dialect ${JSON.stringify(value.dialect)}`,
  );
  assert.ok(['api_key', 'oauth_helper'].includes(value.auth), `${value.id}: auth is invalid`);
  let secret;
  if (value.auth === 'api_key') {
    assert.ok(value.base_url && URL.canParse(value.base_url), `${value.id}: base_url must be absolute`);
    const url = new URL(value.base_url);
    assert.ok(['https:', 'http:'].includes(url.protocol), `${value.id}: base_url must use HTTP(S)`);
    assert.match(value.secret_env ?? '', SAFE_ENV, `${value.id}: secret_env is invalid`);
    assert.ok(optional(env[value.secret_env]), `${value.id}: ${value.secret_env} is not configured`);
    assert.equal(value.oauth_helper, undefined, `${value.id}: api_key auth cannot use oauth_helper`);
    secret = env[value.secret_env];
  } else {
    assert.equal(value.dialect, 'vertex_gemini', `${value.id}: OAuth helper requires vertex_gemini`);
    assert.match(value.oauth_helper ?? '', SAFE_HELPER, `${value.id}: oauth_helper is invalid`);
    assert.equal(value.secret_env, undefined, `${value.id}: OAuth helper cannot select a secret`);
    assert.equal(value.base_url, undefined, `${value.id}: Vertex base_url is server-owned`);
    assert.ok(
      value.configuration && typeof value.configuration === 'object' && !Array.isArray(value.configuration),
      `${value.id}: OAuth helper configuration is required`,
    );
    value.configuration = {
      project_id: optional(value.configuration.project_id),
      location: optional(value.configuration.location) ?? 'global',
    };
    assert.ok(value.configuration.project_id, `${value.id}: project_id is required`);
  }
  assert.ok(
    Number.isInteger(value.timeout_secs) && value.timeout_secs >= 1 && value.timeout_secs <= 900,
    `${value.id}: timeout_secs must be in 1..=900`,
  );
  return Object.freeze({ ...value, secret });
}

export function loadProviderCases(env = process.env) {
  const authored = parseAuthoredCases(env);
  const candidates = authored.length > 0 ? authored : autoCases(env);
  const cases = candidates.map((item) => normalizeCase(item, env));
  const ids = cases.map(({ id }) => id);
  assert.equal(new Set(ids).size, ids.length, 'provider case ids must be unique');
  return cases;
}

export function publicProviderCase(value) {
  const { secret: _secret, ...publicValue } = value;
  return publicValue;
}

export const providerDialects = Object.freeze([...DIALECTS]);
