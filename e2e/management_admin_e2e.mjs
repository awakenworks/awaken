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
import { withServer, pass } from './harness.mjs';

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
  try {
    await withServer('management', 38150, async (base) => {
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
