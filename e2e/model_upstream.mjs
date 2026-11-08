// The one way an e2e gives a scenario process its model upstream: the way an
// operator does it in the console, through the config-plane API of the running
// process. Nothing reaches the process environment.
//
//   1. POST /v1/config/provider-connections — test the provider, discover its
//      offerings and seal the key in the credential vault;
//   2. PUT  /v1/config/inference-profiles/{id} — bind a model and credential.
//
// The scenario process resolves every inference of its fixture Agents through
// the bound profile (`model_configuration.rs` in awaken-scenario-host): the
// profile named by the Agent's model reference, else `default`.
//
// Decision table for one configuration request:
//
// | Rule | provider answers discovery | bound model offered | effect                      |
// |------|----------------------------|---------------------|-----------------------------|
// | U1   | yes                        | yes                 | profiles bound, binding out |
// | U2   | no                         | -                   | throw with the API problem  |
// | U3   | yes                        | no                  | throw: no offering to bind  |

import fs from 'node:fs';
import { loadKimiConfig } from './kimi_config.mjs';

export const DEFAULT_PROFILE = 'default';
// The connection's endpoint qualifier keeps the harness's provider apart from
// any connection a spec authors for the same provider and dialect.
const ENDPOINT_NAME = 'scenario-upstream';

// Carries a model-upstream binding through an env object without becoming a
// variable: spawn() copies only string keys, object spread keeps symbols.
export const MODEL_UPSTREAM = Symbol.for('awaken.e2e.modelUpstream');

// Variables that would configure a model through the environment. A scenario
// process must never be started with any of them (see assertNoModelEnvironment).
export function modelEnvironmentVariables(environment) {
  return Object.keys(environment).filter((name) => {
    const key = name.toUpperCase();
    return key === 'API_KEY' || key.endsWith('_API_KEY') || key === 'AWAKEN_MODEL_SOURCE';
  });
}

export function assertNoModelEnvironment(environment, what = 'a scenario process') {
  const offending = modelEnvironmentVariables(environment);
  if (offending.length > 0) {
    throw new Error(
      `refusing to start ${what} with model configuration in its environment `
      + `(${offending.join(', ')}); configure the model through the config-plane API `
      + '(configureModelUpstream)',
    );
  }
}

// An env fragment that asks the harness to configure `upstream` once the
// process listens. `upstream` is a fake provider fixture or `{ url }`.
export function modelUpstreamEnv(upstream, options = {}) {
  return { [MODEL_UPSTREAM]: { ...(upstream ? { upstream } : {}), ...options } };
}

async function call(baseUrl, method, route, body, authToken) {
  const response = await fetch(`${baseUrl}${route}`, {
    method,
    headers: {
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
      ...(authToken ? { authorization: `Bearer ${authToken}` } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    json = { raw: text };
  }
  return { status: response.status, json };
}

function redact(value, secrets) {
  let text = JSON.stringify(value);
  for (const secret of secrets) if (secret) text = text.split(secret).join('<redacted>');
  return text;
}

/**
 * Configure the running process at `baseUrl` to run its model on `upstream`.
 *
 * @param {string} baseUrl
 * @param {{
 *   upstream?: { url: string, models?: string[] },
 *   baseUrl?: string,
 *   apiKey?: string,
 *   credentialSourceId?: string,
 *   oauthHelper?: string,
 *   configuration?: Record<string, string>,
 *   bindSecret?: string,
 *   bindCredentialSourceId?: string,
 *   authTokenFile?: string,
 *   provider?: string,
 *   dialect?: string,
 *   model?: string,
 *   profiles?: Record<string, string>,
 *   workspaceId?: string,
 *   idempotencyKey?: string,
 * }} options
 *   `upstream` is a fake fixture: its directory is made to offer `model`.
 *   `baseUrl`/`apiKey` name a real provider instead; without `baseUrl` the
 *   provider's catalogued default endpoint applies. `credentialSourceId`
 *   reuses a credential the process already holds; `oauthHelper` names a
 *   server-owned token helper instead of a key, with the provider's
 *   `configuration` fields (Vertex: project and location). `bindSecret` binds the
 *   profiles to a separately entered credential, for a test whose operator
 *   enters a key the provider then refuses; `bindCredentialSourceId` binds
 *   them to a credential the process already holds. `authTokenFile` names the
 *   file an IAM-secured process wrote its bootstrap admin token to; the
 *   operator authenticates with it. `profiles` maps profile id to
 *   model; it defaults to `{ default: model }`.
 */
export async function configureModelUpstream(baseUrl, options = {}) {
  const {
    upstream,
    apiKey = upstream ? FAKE_UPSTREAM_KEY : undefined,
    credentialSourceId,
    oauthHelper,
    configuration,
    bindSecret,
    bindCredentialSourceId,
    authTokenFile,
    provider = 'anthropic',
    dialect = 'anthropic_messages',
    model = 'fake-haiku',
    idempotencyKey = `e2e-model-upstream-${provider}-${dialect}`,
  } = options;
  const profiles = options.profiles ?? { [DEFAULT_PROFILE]: model };
  const providerBase = options.baseUrl ?? (upstream ? `${upstream.url}/v1/` : undefined);
  if (upstream?.models) {
    for (const offered of Object.values(profiles)) {
      if (!upstream.models.includes(offered)) upstream.models.push(offered);
    }
  }
  const secrets = [apiKey, bindSecret];
  const token = authTokenFile ? fs.readFileSync(authTokenFile, 'utf8').trim() : undefined;
  // An unauthenticated operator claims no Workspace (the edge resolves it); a
  // token-scoped one must name the Workspace its token is scoped to.
  let workspaceId = options.workspaceId ?? '';
  if (options.workspaceId === undefined && token) {
    const context = await call(baseUrl, 'GET', '/v1/config/workspace-context', undefined, token);
    workspaceId = context.status === 200 ? context.json?.workspace_id ?? '' : '';
  }
  const connection = await call(baseUrl, 'POST', '/v1/config/provider-connections', {
    idempotency_key: idempotencyKey,
    workspace_id: workspaceId,
    provider_id: provider,
    display_name: `E2E ${provider}`,
    dialect,
    endpoint_name: ENDPOINT_NAME,
    ...(providerBase ? { base_url: providerBase } : {}),
    ...(configuration ? { configuration } : {}),
    timeout_secs: 300,
    ...(credentialSourceId
      ? { credential_source_id: credentialSourceId }
      : oauthHelper ? { oauth_helper: oauthHelper } : { secret: apiKey }),
  }, token);
  // 200 is the idempotent replay a restarted durable process answers.
  if (connection.status !== 201 && connection.status !== 200) {
    throw new Error(
      `provider connection failed (${connection.status}): ${redact(connection.json, secrets)}`,
    );
  }
  const endpointId = connection.json.endpoint.id;
  let bound = bindCredentialSourceId ?? connection.json.credential.id;
  if (bindSecret !== undefined) {
    const entered = await call(baseUrl, 'POST', '/v1/config/credentials', {
      workspace_id: workspaceId,
      kind: 'vault',
      provider_id: provider,
      secret: bindSecret,
    }, token);
    if (entered.status !== 201) {
      throw new Error(`credential entry failed (${entered.status}): ${redact(entered.json, secrets)}`);
    }
    bound = entered.json.id;
  }
  const catalog = await call(baseUrl, 'GET', '/v1/config/catalog', undefined, token);
  for (const [profileId, profileModel] of Object.entries(profiles)) {
    const offered = (catalog.json?.offerings ?? []).some((offering) =>
      offering.model_id === profileModel && offering.protocol_endpoint_id === endpointId);
    if (!offered) {
      throw new Error(`provider ${provider} at ${providerBase ?? endpointId} offers no model ${profileModel}`);
    }
    const profile = await call(baseUrl, 'PUT', `/v1/config/inference-profiles/${profileId}`, {
      workspace_id: workspaceId,
      primary: {
        target: { model_id: profileModel, provider_id: provider, protocol_endpoint_id: endpointId },
        credential_binding: { type: 'exact', credential_source_id: bound },
      },
      fallbacks: [],
      disabled_endpoint_ids: [],
    }, token);
    if (profile.status !== 200) {
      throw new Error(
        `binding profile ${profileId} failed (${profile.status}): ${redact(profile.json, secrets)}`,
      );
    }
  }
  return { endpointId, credentialId: bound, profiles, model };
}

// The key every fake upstream fixture accepts unless a test chooses another.
export const FAKE_UPSTREAM_KEY = 'sk-fake-upstream-key'; // awaken-allow(secret):

// Live providers. The key comes from the operator's local secrets — the test
// controller's environment, or ~/.bashrc for Kimi (kimi_config.mjs) — and
// reaches the product only through configureModelUpstream. Each returns null
// when no key is available, so a real-provider spec can skip.
export function liveAnthropicCompatibleUpstream(env = process.env) {
  const kimi = loadKimiConfig();
  if (!kimi?.anthropicKey) return liveDeepSeekAnthropicUpstream(env);
  const root = kimi.anthropicBase.replace(/\/+$/u, '');
  const kimiCode = root.includes('api.kimi.com/coding');
  return {
    provider: kimiCode ? 'kimi' : 'anthropic',
    dialect: 'anthropic_messages',
    // The Messages API base; Kimi Code's root omits the /v1 the executor needs.
    baseUrl: kimiCode && !root.endsWith('/v1') ? `${root}/v1/` : `${root}/`,
    apiKey: kimi.anthropicKey,
    model: kimi.anthropicModel,
  };
}

export function liveGeminiUpstream(env = process.env) {
  const apiKey = env.GEMINI_API_KEY || env.GOOGLE_API_KEY;
  if (!apiKey) return null;
  return {
    provider: 'gemini',
    dialect: 'gemini',
    baseUrl: 'https://generativelanguage.googleapis.com/v1beta',
    apiKey,
    model: env.GEMINI_MODEL ?? 'gemini-2.5-flash',
  };
}

// Gemini on Vertex AI, authenticated by the server-owned gcloud token helper.
export function liveVertexUpstream(env = process.env) {
  if (!env.GEMINI_PROJECT) return null;
  return {
    provider: 'vertex',
    dialect: 'vertex_gemini',
    oauthHelper: 'gcloud',
    configuration: { project_id: env.GEMINI_PROJECT, location: env.GEMINI_LOCATION ?? 'global' },
    model: env.GEMINI_MODEL ?? 'gemini-2.5-flash',
  };
}

// DeepSeek's Anthropic Messages endpoint, keyed the way the DeepSeek jobs read
// it (DEEPSEEK_API_KEY or DEEPSEEK_API_KEY_FILE): the Anthropic-compatible
// fallback when no Kimi key is configured.
export function liveDeepSeekAnthropicUpstream(env = process.env) {
  const apiKey = env.DEEPSEEK_API_KEY
    || (env.DEEPSEEK_API_KEY_FILE && fs.readFileSync(env.DEEPSEEK_API_KEY_FILE, 'utf8').trim());
  if (!apiKey) return null;
  return {
    provider: 'deepseek',
    dialect: 'anthropic_messages',
    apiKey,
    model: env.DEEPSEEK_MODEL ?? 'deepseek-flash',
  };
}
