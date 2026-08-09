// Admin config-plane e2e driven over HTTP and validated against the **generated
// TS API contract** (ADR-0043). It authors provider / endpoint / offering /
// credential through `/v1/config/*`, reads the catalog back, and dry-runs a binding
// through the resolver (`/v1/config/inference/resolve`). Every response is validated
// against the committed JSON Schema in `contracts/model-schemas.generated.json` (the same SSOT
// the `.d.ts` is generated from) with Ajv — so a drift between the running server
// and the generated contract fails here. This exercises the admin-config-api CRUD +
// config-resolver + model-catalog + credential domains end to end via the wire.
//
// Run: (from e2e/)  npm install && node management_admin_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Ajv2020 } from 'ajv/dist/2020.js';
import { withScenarioServer, pass } from './harness.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const CONTRACT = JSON.parse(
  fs.readFileSync(path.join(REPO_ROOT, 'contracts', 'model-schemas.generated.json'), 'utf8'),
);
const ajv = new Ajv2020({ strict: false, allErrors: true });
const validators = Object.fromEntries(
  Object.entries(CONTRACT.schemas).map(([name, schema]) => [name, ajv.compile(schema)]),
);

/// Assert a value matches the generated schema `name`; throw with the Ajv errors.
function checkContract(name, value) {
  const validate = validators[name];
  assert.ok(validate, `no generated schema for ${name}`);
  const ok = validate(value);
  assert.ok(ok, `${name} response violates the generated contract: ${ajv.errorsText(validate.errors)}`);
}

async function req(base, method, uri, body) {
  const res = await fetch(`${base}${uri}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let json = null;
  if (text) {
    try {
      json = JSON.parse(text);
    } catch {
      // Axum's extractor rejects malformed request shapes before the API error
      // mapper runs and intentionally returns plain text.
      json = text;
    }
  }
  return { status: res.status, json };
}

async function startModelDirectory(apiKey) {
  const state = {
    models: ['provider-model-a', 'provider-model-b'],
    requests: [],
  };
  const server = http.createServer((request, response) => {
    state.requests.push({
      method: request.method,
      url: request.url,
      apiKey: request.headers['x-api-key']
        ?? request.headers.authorization?.replace(/^Bearer\s+/u, ''),
    });
    const suppliedKey = request.headers['x-api-key']
      ?? request.headers.authorization?.replace(/^Bearer\s+/u, '');
    if (
      request.method !== 'GET'
      || !request.url.startsWith('/v1/models')
      || suppliedKey !== apiKey
    ) {
      response.writeHead(401, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { message: 'unauthorized' } }));
      return;
    }
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({
      data: state.models.map((id) => ({ id })),
      has_more: false,
    }));
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  assert.ok(address && typeof address === 'object');
  return {
    state,
    url: `http://127.0.0.1:${address.port}`,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

async function main() {
  process.env.ANTHROPIC_API_KEY = 'must-not-be-config-input'; // awaken-allow: secret
  process.env.ANTHROPIC_MODEL = 'must-not-be-selected';
  const directory = await startModelDirectory('sk-admin-e2e'); // awaken-allow: secret
  try {
    await withScenarioServer('management', 'mcp', 38150, async (base) => {
      // Cause/effect decision table for G36's single persisted input path:
      // E1 provider env present -> proposal route absent and catalog unchanged;
      // E2 explicit Provider Connection write -> catalog/credential facts appear.
      let r = await req(base, 'GET', '/v1/config/provider-proposals');
      assert.equal(r.status, 404, 'E1 environment proposal path is removed');
      const catalogBefore = await req(base, 'GET', '/v1/config/catalog');
      assert.equal(catalogBefore.json.offerings.length, 0, 'E1 env cannot author catalog truth');

      // Provider Connections cause graph:
      // C1 dialect supported by the provider template; C2 non-empty secret;
      // C3 live discovery succeeds; C4 discovery returns models.
      // E1 reject before persistence; E2 atomically persist provider, endpoint,
      // credential, and offerings; E3 expose only a secret-free Ready summary.
      //
      // Decision table:
      // | Rule | C1 | C2 | C3 | C4 | Expected |
      // | R1   | N  | -  | -  | -  | 422 dialect_unsupported, E1  |
      // | R2   | Y  | N  | -  | -  | 422 invalid credential, E1  |
      // | R3   | Y  | Y  | N  | -  | upstream error, E1           |
      // | R4   | Y  | Y  | Y  | N  | 422 no_models_discovered, E1|
      // | R5   | Y  | Y  | Y  | Y  | 201, E2 + E3                 |
      r = await req(base, 'GET', '/v1/config/provider-descriptors');
      assert.equal(r.status, 200, JSON.stringify(r.json));
      const anthropicDescriptor = r.json.find((item) => item.provider_kind === 'anthropic');
      assert.ok(anthropicDescriptor);
      checkContract('ProviderDriverDescriptor', anthropicDescriptor);
      assert.ok(anthropicDescriptor.supported_dialects.includes('anthropic_messages'));
      const deepseekDescriptor = r.json.find((item) => item.provider_kind === 'deepseek');
      assert.ok(deepseekDescriptor);
      // Descriptor decision rules: D1 DeepSeek + OpenAI-compatible dialect ->
      // native base; D2 DeepSeek + Anthropic dialect -> `/anthropic` inference
      // base while model discovery remains provider-native. Both are owned by
      // the model-catalog descriptor; the e2e must not retain the older one-arm list.
      assert.deepEqual(deepseekDescriptor.supported_dialects, ['open_ai_chat', 'anthropic_messages']);
      assert.equal(deepseekDescriptor.default_endpoints[0].base_url, 'https://api.deepseek.com');
      assert.equal(deepseekDescriptor.default_endpoints[1].base_url, 'https://api.deepseek.com/anthropic');
      assert.equal(deepseekDescriptor.default_endpoints[1].model_discovery_base_url, 'https://api.deepseek.com');

      r = await req(base, 'GET', '/v1/config/provider-connections?workspace_id=ws');
      assert.equal(r.status, 200, JSON.stringify(r.json));
      r.json.forEach((summary) => checkContract('ProviderConnectionSummary', summary));
      assert.equal(r.json.find((item) => item.provider_id === 'anthropic').status, 'not_configured');

      const connection = {
        idempotency_key: 'management-admin-provider-connection',
        workspace_id: 'ws',
        provider_id: 'anthropic',
        display_name: 'Anthropic E2E',
        dialect: 'anthropic_messages',
        base_url: `${directory.url}/v1/`,
        timeout_secs: 30,
        secret: 'sk-admin-e2e', // awaken-allow: secret
      };
      r = await req(base, 'POST', '/v1/config/provider-connections', {
        ...connection,
        dialect: 'open_ai_responses',
      });
      assert.equal(r.status, 422);
      assert.equal(r.json.code, 'dialect_unsupported');
      r = await req(base, 'POST', '/v1/config/provider-connections', {
        ...connection,
        secret: '  ',
      });
      assert.equal(r.status, 422);

      r = await req(base, 'POST', '/v1/config/provider-connections', {
        ...connection,
        timeout_secs: undefined,
        secret: 'wrong-secret', // awaken-allow: secret
      });
      assert.equal(r.status, 502, JSON.stringify(r.json));
      assert.equal(r.json.code, 'model_discovery_failed');
      directory.state.models = [];
      r = await req(base, 'POST', '/v1/config/provider-connections', connection);
      assert.equal(r.status, 422, JSON.stringify(r.json));
      assert.equal(r.json.code, 'no_models_discovered');

      directory.state.models = ['connection-model-a', 'connection-model-b'];
      r = await req(base, 'POST', '/v1/config/provider-connections', connection);
      assert.equal(r.status, 201, JSON.stringify(r.json));
      checkContract('ProviderConnectionView', r.json);
      assert.equal(r.json.sync.discovered, 2);
      assert.ok(!JSON.stringify(r.json).includes(connection.secret), 'connection response is secret-free');
      const credId = r.json.credential.id;
      r = await req(base, 'GET', '/v1/config/provider-connections?workspace_id=ws');
      const readyConnection = r.json.find((item) => item.provider_id === 'anthropic');
      checkContract('ProviderConnectionSummary', readyConnection);
      assert.equal(readyConnection.status, 'ready');
      assert.equal(readyConnection.active_credentials, 1);
      assert.equal(readyConnection.active_models, 2);

      // Connection-state cause graph:
      // C1 authored provider; C2 active credential; C3 observed offerings;
      // C4 at least one offering active; C5 last active observation is fresh.
      // E1 NotConfigured, E2 NeedsAttention, E3 Connected, E4 Unavailable,
      // E5 Stale, E6 Ready. Stale needs a historical clock fixture and is kept
      // out of this live-clock table; every other state is driven over HTTP.
      //
      // | Rule | C1 | C2 | C3 | C4 | C5 | Expected |
      // | S1   | N  | N  | N  | -  | -  | not_configured |
      // | S2   | Y  | N  | -  | -  | -  | needs_attention |
      // | S3   | -  | Y  | N  | -  | -  | connected |
      // | S4   | -  | Y  | Y  | N  | -  | unavailable |
      // | S5   | -  | Y  | Y  | Y  | Y  | ready |
      directory.state.models = ['provider-model-a', 'provider-model-b'];
      directory.state.requests.length = 0;
      pass('Provider Connection validates and atomically persists a ready catalog');

      // --- catalog read validates against the generated ProviderCatalog schema ---
      r = await req(base, 'GET', '/v1/config/catalog');
      assert.equal(r.status, 200);
      checkContract('ProviderCatalog', r.json);
      assert.ok('anthropic' in r.json.providers);
      pass('GET /v1/config/catalog matches ProviderCatalog contract');

      // Model-attribute cause graph:
      // C1 context is positive; C2 output limit is positive; C3 output <= context.
      // E1 persist a Manual provenance stamp, otherwise E2 reject the aggregate.
      //
      // | Rule | C1 | C2 | C3 | Expected |
      // | A1   | Y  | Y  | Y  | 200 + manual provenance |
      // | A2   | N  | -  | -  | 422 context_window |
      // | A3   | -  | N  | -  | 422 max_output_tokens |
      // | A4   | Y  | Y  | N  | 422 output exceeds context |
      r = await req(base, 'PUT', '/v1/config/model-attributes/connection-model-a', {
        context_window: 200_000,
        max_output_tokens: 8_192,
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      checkContract('ModelAttributes', r.json);
      assert.equal(r.json.provenance.context_window.source, 'manual');
      assert.equal(r.json.provenance.max_output_tokens.source, 'manual');
      for (const [attributes, detail] of [
        [{ context_window: 0 }, 'context_window'],
        [{ max_output_tokens: 0 }, 'max_output_tokens'],
        [{ context_window: 10, max_output_tokens: 11 }, 'max_output_tokens cannot exceed context_window'],
      ]) {
        r = await req(base, 'PUT', `/v1/config/model-attributes/invalid-${detail.length}`, attributes);
        assert.equal(r.status, 422, JSON.stringify(r.json));
        assert.equal(r.json.code, 'catalog_invariant');
        assert.match(r.json.detail, new RegExp(detail));
      }
      pass('manual model attributes persist provenance and reject all numeric invariant violations');

      r = await req(base, 'GET', `/v1/config/credentials/${credId}`);
      assert.equal(r.status, 200);
      checkContract('CredentialSource', r.json);
      assert.ok(!JSON.stringify(r.json).includes(connection.secret), 'stored credential stays secret-free');

      // --- resolve dry-run through the resolver, validated against the contract ---
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws',
        target: { model_id: 'connection-model-a' },
        binding: { type: 'exact', credential_source_id: credId },
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      checkContract('ResolvedInferenceView', r.json);
      assert.equal(r.json.provider_id, 'anthropic');
      assert.equal(r.json.adapter_kind, 'anthropic');
      assert.equal(r.json.base_url, `${directory.url}/v1/`);
      assert.equal(r.json.credential_present, true);
      pass('POST /v1/config/inference/resolve — resolver output matches ResolvedInferenceView contract');

      // Target-shape boundary: `target` is the sole contract. The retired
      // top-level `model_id` and a missing target both fail before resolution.
      for (const request of [
        {
          workspace_id: 'ws', model_id: 'connection-model-a',
          target: { model_id: 'connection-model-a' }, binding: { type: 'none' },
        },
        { workspace_id: 'ws', binding: { type: 'none' } },
      ]) {
        r = await req(base, 'POST', '/v1/config/inference/resolve', request);
        assert.equal(r.status, 422, JSON.stringify(r.json));
      }
      pass('resolve request rejects retired or missing target shapes at the HTTP boundary');

      // --- credential read-back + list (validated) ---
      r = await req(base, 'GET', `/v1/config/credentials/${credId}`);
      assert.equal(r.status, 200);
      checkContract('CredentialSource', r.json);
      assert.ok(!JSON.stringify(r.json).includes('sk-admin-e2e'), 'GET credential is secret-free');

      r = await req(base, 'GET', '/v1/config/credentials?workspace_id=ws');
      assert.equal(r.status, 200);
      assert.ok(Array.isArray(r.json) && r.json.some((c) => c.id === credId), 'list contains the credential');
      r.json.forEach((c) => checkContract('CredentialSource', c));
      pass('GET credential + list — all read-backs match the contract');

      // --- credential error arm ---
      r = await req(base, 'GET', '/v1/config/credentials/cred_missing');
      assert.equal(r.status, 404);
      assert.equal(r.json.code, 'not_found');
      pass('unknown credential -> 404');

      // --- resolve with an Exact binding to a missing credential fails closed ---
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws',
        target: { model_id: 'connection-model-a' },
        binding: { type: 'exact', credential_source_id: 'cred_missing' },
      });
      assert.equal(r.status, 404, JSON.stringify(r.json));
      pass('resolve with a missing credential binding -> 404');

      // --- an `env` credential is rejected: proposal must be explicitly persisted ---
      r = await req(base, 'POST', '/v1/config/credentials', {
        workspace_id: 'ws', kind: 'env', provider_id: 'anthropic', env_key: 'ANTHROPIC_API_KEY',
      });
      assert.equal(r.status, 422);
      assert.equal(r.json.code, 'credential_invalid');
      pass('env-kind credential is never admitted as execution truth');

      // --- credential pool + failover resolve ---------------------------------
      // Member A is a persisted credential that is archived before resolution.
      r = await req(base, 'POST', '/v1/config/credentials', {
        workspace_id: 'ws', kind: 'vault', provider_id: 'anthropic', secret: 'sk-disabled', // awaken-allow: secret
      });
      assert.equal(r.status, 201);
      const badCredId = r.json.id;
      r = await req(base, 'POST', `/v1/config/credentials/${badCredId}/archive`);
      assert.equal(r.status, 200);

      // Author a pool: A (ordinal 0, will fail) then the good vault credential B.
      r = await req(base, 'PUT', '/v1/config/credential-pools/pool1', {
        id: 'pool1', workspace_id: 'ws',
        members: [
          { credential_source_id: badCredId, ordinal: 0, enabled: true, selection_weight: 0 },
          { credential_source_id: credId, ordinal: 1, enabled: true, selection_weight: 0 },
        ],
      });
      assert.equal(r.status, 200);
      checkContract('CredentialPool', r.json);

      r = await req(base, 'GET', '/v1/config/credential-pools/pool1');
      assert.equal(r.status, 200);
      checkContract('CredentialPool', r.json);
      assert.equal(r.json.members.length, 2);
      pass('PUT/GET credential-pool match the generated CredentialPool contract');

      // Resolve the pool binding: A (ordinal 0) fails to materialize -> fail over to B.
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws', target: { model_id: 'connection-model-a' },
        binding: { type: 'one_of_credential_pool', credential_pool_id: 'pool1' },
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      checkContract('ResolvedInferenceView', r.json);
      assert.equal(r.json.credential_present, true, 'failover reached a materializable member');
      pass('pool resolve fails over past an unmaterializable member -> credential_present=true');

      // A pool whose only member fails to materialize -> exhausted (409).
      r = await req(base, 'PUT', '/v1/config/credential-pools/pool_bad', {
        id: 'pool_bad', workspace_id: 'ws',
        members: [{ credential_source_id: badCredId, ordinal: 0, enabled: true, selection_weight: 0 }],
      });
      assert.equal(r.status, 200);
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws', target: { model_id: 'connection-model-a' },
        binding: { type: 'one_of_credential_pool', credential_pool_id: 'pool_bad' },
      });
      assert.equal(r.status, 409);
      assert.equal(r.json.code, 'pool_exhausted');
      pass('pool with no materializable member -> 409 pool_exhausted');

      // A binding to a pool that does not exist -> missing (404).
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws', target: { model_id: 'connection-model-a' },
        binding: { type: 'one_of_credential_pool', credential_pool_id: 'pool_missing' },
      });
      assert.equal(r.status, 404);
      assert.equal(r.json.code, 'not_found');
      pass('binding to an unknown pool -> 404 not_found');

      // GET a missing pool -> 404.
      r = await req(base, 'GET', '/v1/config/credential-pools/pool_missing');
      assert.equal(r.status, 404);
      pass('GET unknown credential-pool -> 404');

      // --- InferenceProfile CRUD + resolve ----------------------------------------
      // Profile-validation cause graph:
      // C1 primary model non-empty; C2 fallback count <= 8; C3 every fallback
      // model non-empty; C4 every structured target unique. Only C1..C4 persists.
      // | Rule | C1 | C2 | C3 | C4 | Expected |
      // | P1   | N  | -  | -  | -  | 422 empty primary |
      // | P2   | Y  | N  | -  | -  | 422 too many fallbacks |
      // | P3   | Y  | Y  | N  | -  | 422 empty fallback |
      // | P4   | Y  | Y  | Y  | N  | 422 duplicate target |
      const candidate = (model_id) => ({
        target: { model_id }, credential_binding: { type: 'none' },
      });
      const invalidProfiles = [
        { primary: candidate('  '), fallbacks: [], disabled_endpoint_ids: [] },
        {
          primary: candidate('primary'),
          fallbacks: Array.from({ length: 9 }, (_, index) => candidate(`fallback-${index}`)),
          disabled_endpoint_ids: [],
        },
        { primary: candidate('primary'), fallbacks: [candidate(' ')], disabled_endpoint_ids: [] },
        { primary: candidate('duplicate'), fallbacks: [candidate('duplicate')], disabled_endpoint_ids: [] },
      ];
      for (const [index, profile] of invalidProfiles.entries()) {
        r = await req(base, 'PUT', `/v1/config/inference-profiles/invalid-${index}`, profile);
        assert.equal(r.status, 422, JSON.stringify(r.json));
        assert.equal(r.json.code, 'invalid_inference_profile');
      }
      pass('inference-profile authoring rejects every invalid cause partition atomically');

      r = await req(base, 'PUT', '/v1/config/inference-profiles/prof1', {
        primary: {
          target: {
            model_id: 'connection-model-a', provider_id: 'anthropic', protocol_endpoint_id: 'anthropic.anthropic_messages',
          },
          credential_binding: { type: 'exact', credential_source_id: credId },
        },
        fallbacks: [],
        disabled_endpoint_ids: [],
      });
      assert.equal(r.status, 200);
      checkContract('InferenceProfile', r.json);

      r = await req(base, 'GET', '/v1/config/inference-profiles/prof1');
      assert.equal(r.status, 200);
      checkContract('InferenceProfile', r.json);
      pass('PUT/GET inference-profile match the generated InferenceProfile contract');

      r = await req(base, 'POST', '/v1/config/inference-profiles/prof1/resolve', { workspace_id: 'ws' });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      checkContract('ResolvedInferenceView', r.json);
      assert.equal(r.json.base_url, `${directory.url}/v1/`);
      assert.equal(r.json.credential_present, true);
      pass('resolve-by-profile selects the explicit primary connection');

      r = await req(base, 'POST', '/v1/config/inference-profiles/nope/resolve', { workspace_id: 'ws' });
      assert.equal(r.status, 404);
      pass('resolve of an unknown profile -> 404');

      // --- archive a credential -> it fails closed at resolution -------------------
      r = await req(base, 'POST', `/v1/config/credentials/${credId}/archive`, {});
      assert.equal(r.status, 200);
      checkContract('CredentialSource', r.json);
      assert.equal(r.json.status, 'disabled');
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws',
        target: {
          model_id: 'connection-model-a', provider_id: 'anthropic', protocol_endpoint_id: 'anthropic.anthropic_messages',
        },
        binding: { type: 'exact', credential_source_id: credId },
      });
      assert.equal(r.status, 422, JSON.stringify(r.json));
      assert.equal(r.json.code, 'credential_invalid');
      pass('archived credential -> resolve fails closed (422 credential_invalid)');

      // --- resolve with a None binding returns a triple with no credential ---
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws',
        target: {
          model_id: 'connection-model-a', provider_id: 'anthropic', protocol_endpoint_id: 'anthropic.anthropic_messages',
        },
        binding: { type: 'none' },
      });
      assert.equal(r.status, 200);
      checkContract('ResolvedInferenceView', r.json);
      assert.equal(r.json.credential_present, false);
      pass('resolve with a None binding -> triple, credential_present=false');

      // --- a binding to an unknown model fails closed (404) ---
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws',
        target: { model_id: 'no-such-model' },
        binding: { type: 'none' },
      });
      assert.equal(r.status, 404);
      assert.equal(r.json.code, 'model_unresolved');
      pass('resolve of an unknown model -> 404 model_unresolved');
    });

    console.log('E2E PASS: admin config CRUD + resolve round-trip against the generated TS API contract.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await directory.close();
    delete process.env.ANTHROPIC_API_KEY;
    delete process.env.ANTHROPIC_BASE_URL;
    delete process.env.ANTHROPIC_MODEL;
  }
}

main();
