// Live credential-validation e2e (ADR-0043): drives the admin
// `POST /v1/config/credentials/:id/validate` route, which resolves a credential to
// its endpoint and live-probes the provider through the injected `CredentialProbe`
// port (server-local backs it with provider-genai). Validated against the generated
// `CredentialValidation` contract. Gated on a live Anthropic-compatible key.
//
// Run: (from e2e/, with a live key)
//   ANTHROPIC_API_KEY=... ANTHROPIC_BASE_URL=... ANTHROPIC_MODEL=... node management_validate_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Ajv2020 } from 'ajv/dist/2020.js';
import { withServer, pass } from './harness.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const CONTRACT = JSON.parse(fs.readFileSync(path.join(REPO_ROOT, 'contracts', 'model-schemas.json'), 'utf8'));
const ajv = new Ajv2020({ strict: false });
const validateContract = ajv.compile(CONTRACT.schemas.CredentialValidation);

async function req(base, method, uri, body) {
  const res = await fetch(`${base}${uri}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  return { status: res.status, json: text ? JSON.parse(text) : null };
}

async function main() {
  const key = process.env.ANTHROPIC_API_KEY || process.env.KIMI_API_KEY;
  if (!key) {
    console.log('SKIP management_validate_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  const baseUrl = process.env.ANTHROPIC_BASE_URL || process.env.KIMI_BASE_URL || 'https://api.anthropic.com/v1/';
  const model = process.env.ANTHROPIC_MODEL || process.env.KIMI_MODEL || 'claude-3-5-haiku-latest';

  try {
    await withServer('management', 38160, async (base) => {
      // Author a provider + endpoint (the live base URL) + offering for `model`.
      await req(base, 'PUT', '/v1/config/providers/anthropic', {
        id: 'anthropic', slug: 'anthropic', display_name: 'Anthropic', version: 1,
      });
      await req(base, 'PUT', '/v1/config/endpoints/ep1', {
        id: 'ep1', provider_id: 'anthropic', flavor: 'anthropic_messages',
        base_url: baseUrl, timeout_secs: 300, display_name: 'live', version: 1,
      });
      await req(base, 'POST', '/v1/config/offerings', {
        model_id: model, provider_id: 'anthropic',
        protocol_endpoint_id: 'ep1', flavor: 'anthropic_messages', upstream_model: null,
      });

      // A real key validates as `valid`.
      let r = await req(base, 'POST', '/v1/config/credentials', {
        workspace_id: 'ws', kind: 'vault', provider_id: 'anthropic', env_key: 'ANTHROPIC_API_KEY', secret: key,
      });
      assert.equal(r.status, 201);
      const goodId = r.json.id;
      r = await req(base, 'POST', `/v1/config/credentials/${goodId}/validate`, { workspace_id: 'ws', model_id: model });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      assert.ok(validateContract(r.json), `CredentialValidation contract: ${ajv.errorsText(validateContract.errors)}`);
      assert.equal(r.json.status, 'valid', `real key must validate; got ${JSON.stringify(r.json)}`);
      pass('validate a real credential -> status=valid (live probe via provider-genai port)');

      // A bogus key validates as `invalid` (the provider rejects it).
      r = await req(base, 'POST', '/v1/config/credentials', {
        workspace_id: 'ws', kind: 'vault', provider_id: 'anthropic', env_key: 'ANTHROPIC_API_KEY',
        secret: 'sk-obviously-not-a-real-key', // awaken-allow: secret
      });
      assert.equal(r.status, 201);
      const badId = r.json.id;
      r = await req(base, 'POST', `/v1/config/credentials/${badId}/validate`, { workspace_id: 'ws', model_id: model });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      assert.equal(r.json.status, 'invalid', `bogus key must be invalid; got ${JSON.stringify(r.json)}`);
      pass('validate a bogus credential -> status=invalid (never a false valid)');
    });

    console.log('E2E PASS: live credential validation via the probe port + generated CredentialValidation contract.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
