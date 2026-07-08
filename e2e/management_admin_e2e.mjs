// Admin config-plane e2e driven over HTTP and validated against the **generated
// TS API contract** (ADR-0043). It authors provider / endpoint / offering /
// credential through `/v1/config/*`, reads the catalog back, and dry-runs a binding
// through the resolver (`/v1/config/inference/resolve`). Every response is validated
// against the committed JSON Schema in `contracts/model-schemas.json` (the same SSOT
// the `.d.ts` is generated from) with Ajv — so a drift between the running server
// and the generated contract fails here. This exercises the admin-config-api CRUD +
// config-resolver + model-catalog + credential domains end to end via the wire.
//
// Run: (from e2e/)  npm install && node management_admin_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Ajv2020 } from 'ajv/dist/2020.js';
import { withScenarioServer, pass } from './harness.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const CONTRACT = JSON.parse(
  fs.readFileSync(path.join(REPO_ROOT, 'contracts', 'model-schemas.json'), 'utf8'),
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
  const json = text ? JSON.parse(text) : null;
  return { status: res.status, json };
}

async function main() {
  // An `env` credential materializes from a host env var; set one the spawned
  // server inherits so the resolver can read it back (covers the Env branch).
  process.env.AWAKEN_E2E_ENVKEY = 'sk-env-e2e-value'; // awaken-allow: secret
  try {
    await withScenarioServer('management', 'mcp', 38150, async (base) => {
      // --- author provider / endpoint / offering (path id is authoritative) ---
      let r = await req(base, 'PUT', '/v1/config/providers/anthropic', {
        id: 'anthropic',
        slug: 'anthropic',
        display_name: 'Anthropic',
        version: 1,
      });
      assert.equal(r.status, 200);
      checkContract('Provider', r.json);

      r = await req(base, 'PUT', '/v1/config/endpoints/ep1', {
        id: 'ep1',
        provider_id: 'anthropic',
        flavor: 'anthropic_messages',
        base_url: 'https://api.anthropic.com/v1/',
        timeout_secs: 300,
        display_name: 'prod',
        version: 1,
      });
      assert.equal(r.status, 200);
      checkContract('ProtocolEndpoint', r.json);

      r = await req(base, 'POST', '/v1/config/offerings', {
        model_id: 'claude-opus-4-8',
        provider_id: 'anthropic',
        protocol_endpoint_id: 'ep1',
        flavor: 'anthropic_messages',
        upstream_model: null,
      });
      assert.equal(r.status, 200);
      checkContract('Offering', r.json);
      pass('authored provider/endpoint/offering — each response matches the generated schema');

      // --- catalog read validates against the generated ProviderCatalog schema ---
      r = await req(base, 'GET', '/v1/config/catalog');
      assert.equal(r.status, 200);
      checkContract('ProviderCatalog', r.json);
      assert.ok('anthropic' in r.json.providers);
      pass('GET /v1/config/catalog matches ProviderCatalog contract');

      // --- credential entry: secret-in, secret-free-out (validated) ---
      r = await req(base, 'POST', '/v1/config/credentials', {
        workspace_id: 'ws',
        kind: 'vault',
        provider_id: 'anthropic',
        env_key: 'ANTHROPIC_API_KEY',
        secret: 'sk-admin-e2e', // awaken-allow: secret
      });
      assert.equal(r.status, 201);
      checkContract('CredentialSource', r.json);
      assert.ok(!JSON.stringify(r.json).includes('sk-admin-e2e'), 'secret must not be echoed');
      const credId = r.json.id;
      pass('POST /v1/config/credentials — secret-free CredentialSource matches contract');

      // --- resolve dry-run through the resolver, validated against the contract ---
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws',
        model_id: 'claude-opus-4-8',
        binding: { type: 'exact', credential_source_id: credId },
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      checkContract('ResolvedInferenceView', r.json);
      assert.equal(r.json.provider_id, 'anthropic');
      assert.equal(r.json.adapter_kind, 'anthropic');
      assert.equal(r.json.base_url, 'https://api.anthropic.com/v1/');
      assert.equal(r.json.credential_present, true);
      pass('POST /v1/config/inference/resolve — resolver output matches ResolvedInferenceView contract');

      // --- read-back: GET provider / endpoint / credential + list (validated) ---
      r = await req(base, 'GET', '/v1/config/providers/anthropic');
      assert.equal(r.status, 200);
      checkContract('Provider', r.json);
      assert.equal(r.json.slug, 'anthropic');

      r = await req(base, 'GET', '/v1/config/endpoints/ep1');
      assert.equal(r.status, 200);
      checkContract('ProtocolEndpoint', r.json);
      assert.equal(r.json.provider_id, 'anthropic');

      r = await req(base, 'GET', `/v1/config/credentials/${credId}`);
      assert.equal(r.status, 200);
      checkContract('CredentialSource', r.json);
      assert.ok(!JSON.stringify(r.json).includes('sk-admin-e2e'), 'GET credential is secret-free');

      r = await req(base, 'GET', '/v1/config/credentials?workspace_id=ws');
      assert.equal(r.status, 200);
      assert.ok(Array.isArray(r.json) && r.json.some((c) => c.id === credId), 'list contains the credential');
      r.json.forEach((c) => checkContract('CredentialSource', c));
      pass('GET provider/endpoint/credential + list — all read-backs match the contract');

      // --- error arms: 404s + a dangling-reference offering ---
      r = await req(base, 'GET', '/v1/config/providers/no-such-provider');
      assert.equal(r.status, 404);
      assert.equal(r.json.code, 'not_found');

      r = await req(base, 'GET', '/v1/config/endpoints/no-such-endpoint');
      assert.equal(r.status, 404);

      r = await req(base, 'GET', '/v1/config/credentials/cred_missing');
      assert.equal(r.status, 404);
      assert.equal(r.json.code, 'not_found');

      // An offering that references an endpoint that doesn't exist fails closed.
      r = await req(base, 'POST', '/v1/config/offerings', {
        model_id: 'ghost', provider_id: 'anthropic',
        protocol_endpoint_id: 'no-such-endpoint', flavor: 'anthropic_messages', upstream_model: null,
      });
      assert.ok(r.status === 404 || r.status === 422, `dangling offering rejected, got ${r.status}`);
      pass('error arms: unknown provider/endpoint/credential -> 404; dangling offering -> 4xx');

      // --- resolve with an Exact binding to a missing credential fails closed ---
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws',
        model_id: 'claude-opus-4-8',
        binding: { type: 'exact', credential_source_id: 'cred_missing' },
      });
      assert.equal(r.status, 404, JSON.stringify(r.json));
      pass('resolve with a missing credential binding -> 404');

      // --- an `env` credential materializes from the host environment ---
      r = await req(base, 'POST', '/v1/config/credentials', {
        workspace_id: 'ws', kind: 'env', provider_id: 'anthropic', env_key: 'AWAKEN_E2E_ENVKEY',
      });
      assert.equal(r.status, 201);
      checkContract('CredentialSource', r.json);
      assert.equal(r.json.kind, 'env');
      const envCredId = r.json.id;
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws',
        model_id: 'claude-opus-4-8',
        binding: { type: 'exact', credential_source_id: envCredId },
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      checkContract('ResolvedInferenceView', r.json);
      assert.equal(r.json.credential_present, true, 'env credential materialized from the host var');
      pass('env-kind credential resolves by reading the host environment variable');

      // --- credential pool + failover resolve ---------------------------------
      // Member A is an env credential bound to an UNSET var -> materialize fails.
      r = await req(base, 'POST', '/v1/config/credentials', {
        workspace_id: 'ws', kind: 'env', provider_id: 'anthropic', env_key: 'AWAKEN_E2E_UNSET_VAR',
      });
      assert.equal(r.status, 201);
      const badCredId = r.json.id;

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
        workspace_id: 'ws', model_id: 'claude-opus-4-8',
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
        workspace_id: 'ws', model_id: 'claude-opus-4-8',
        binding: { type: 'one_of_credential_pool', credential_pool_id: 'pool_bad' },
      });
      assert.equal(r.status, 409);
      assert.equal(r.json.code, 'pool_exhausted');
      pass('pool with no materializable member -> 409 pool_exhausted');

      // A binding to a pool that does not exist -> missing (404).
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws', model_id: 'claude-opus-4-8',
        binding: { type: 'one_of_credential_pool', credential_pool_id: 'pool_missing' },
      });
      assert.equal(r.status, 404);
      assert.equal(r.json.code, 'not_found');
      pass('binding to an unknown pool -> 404 not_found');

      // GET a missing pool -> 404.
      r = await req(base, 'GET', '/v1/config/credential-pools/pool_missing');
      assert.equal(r.status, 404);
      pass('GET unknown credential-pool -> 404');

      // --- catalog invariant: an offering whose flavor mismatches its endpoint ---
      r = await req(base, 'POST', '/v1/config/offerings', {
        model_id: 'mismatch', provider_id: 'anthropic',
        protocol_endpoint_id: 'ep1', flavor: 'open_ai_chat', upstream_model: null,
      });
      assert.equal(r.status, 422, JSON.stringify(r.json));
      assert.equal(r.json.code, 'catalog_invariant');
      pass('offering flavor mismatch -> 422 catalog_invariant');

      // --- InferenceProfile CRUD + resolve (incl. disabled-endpoint toggle) --------
      // A second endpoint + offering for the same model, so a profile can steer away
      // from ep1 by disabling it.
      await req(base, 'PUT', '/v1/config/endpoints/ep2', {
        id: 'ep2', provider_id: 'anthropic', flavor: 'anthropic_messages',
        base_url: 'https://ep2.example/v1/', timeout_secs: 300, display_name: 'backup', version: 1,
      });
      await req(base, 'POST', '/v1/config/offerings', {
        model_id: 'claude-opus-4-8', provider_id: 'anthropic',
        protocol_endpoint_id: 'ep2', flavor: 'anthropic_messages', upstream_model: null,
      });

      r = await req(base, 'PUT', '/v1/config/inference-profiles/prof1', {
        model_id: 'claude-opus-4-8',
        credential_binding: { type: 'exact', credential_source_id: credId },
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
      assert.equal(r.json.base_url, 'https://api.anthropic.com/v1/');
      assert.equal(r.json.credential_present, true);
      pass('resolve-by-profile picks the first offering (ep1)');

      // A profile that disables ep1 steers resolution to ep2.
      await req(base, 'PUT', '/v1/config/inference-profiles/prof2', {
        model_id: 'claude-opus-4-8',
        credential_binding: { type: 'none' },
        disabled_endpoint_ids: ['ep1'],
      });
      r = await req(base, 'POST', '/v1/config/inference-profiles/prof2/resolve', { workspace_id: 'ws' });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      assert.equal(r.json.base_url, 'https://ep2.example/v1/', 'disabled ep1 -> resolves to ep2');
      pass('resolve-by-profile honors disabled_endpoint_ids (fails over to ep2)');

      r = await req(base, 'POST', '/v1/config/inference-profiles/nope/resolve', { workspace_id: 'ws' });
      assert.equal(r.status, 404);
      pass('resolve of an unknown profile -> 404');

      // --- archive a credential -> it fails closed at resolution -------------------
      r = await req(base, 'POST', `/v1/config/credentials/${credId}/archive`, {});
      assert.equal(r.status, 200);
      checkContract('CredentialSource', r.json);
      assert.equal(r.json.status, 'disabled');
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws', model_id: 'claude-opus-4-8',
        binding: { type: 'exact', credential_source_id: credId },
      });
      assert.equal(r.status, 422, JSON.stringify(r.json));
      assert.equal(r.json.code, 'credential_invalid');
      pass('archived credential -> resolve fails closed (422 credential_invalid)');

      // --- resolve with a None binding returns a triple with no credential ---
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws',
        model_id: 'claude-opus-4-8',
        binding: { type: 'none' },
      });
      assert.equal(r.status, 200);
      checkContract('ResolvedInferenceView', r.json);
      assert.equal(r.json.credential_present, false);
      pass('resolve with a None binding -> triple, credential_present=false');

      // --- a binding to an unknown model fails closed (404) ---
      r = await req(base, 'POST', '/v1/config/inference/resolve', {
        workspace_id: 'ws',
        model_id: 'no-such-model',
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
  }
}

main();
