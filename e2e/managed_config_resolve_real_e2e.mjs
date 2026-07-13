// Config-plane (system config) authoring → resolve → LIVE validate, end-to-end
// over HTTP against awaken-server in `management` mode (which wires the
// admin config plane `/v1/config/*` AND the live GenaiProbe).
//
// The distinction from managed_reconnect_real / managed_real: the model + credential
// are NOT passed to the server as environment variables. They are ENTERED INTO THE
// SYSTEM CONFIG through the HTTP admin plane — provider + endpoint + offering + a
// vault credential (the secret crosses the wire exactly once, on POST) — and then:
//   • POST /v1/config/inference/resolve  -> the secret-free resolved triple
//     (adapter_kind, base_url, credential_present) proves the wiring resolves.
//   • POST /v1/config/credentials/:id/validate -> a LIVE probe drives the real KIMI
//     model with the HTTP-entered secret, proving the config works end to end.
//
// The KIMI key is only the SOURCE of the secret this test enters; the server never
// reads it from the environment. Run (from e2e/, with the ~/.bashrc KIMI config —
// base ends in /v1/, the probe posts to {base_url}messages):
//   ANTHROPIC_API_KEY=sk-kimi-... \
//   ANTHROPIC_BASE_URL=https://api.kimi.com/coding/v1/ \
//   ANTHROPIC_MODEL=kimi-k2-0711-preview \
//   node managed_config_resolve_real_e2e.mjs

import assert from 'node:assert/strict';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38155);
const WS = 'ws';

const KEY = process.env.ANTHROPIC_API_KEY ?? process.env.KIMI_API_KEY;
const BASE = process.env.ANTHROPIC_BASE_URL ?? process.env.KIMI_BASE_URL ?? 'https://api.kimi.com/coding/v1/';
const MODEL = process.env.ANTHROPIC_MODEL ?? process.env.KIMI_MODEL ?? 'kimi-k2-0711-preview';

async function main() {
  if (!KEY) {
    console.log('SKIP managed_config_resolve_real_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  try {
    await withServer('management', PORT, async (baseUrl) => {
      // The config plane returns plain JSON on success and RFC-9457 problem+json on
      // error — not the managed envelope — so drive it with raw fetch.
      const cfg = async (method, path, body) => {
        const res = await fetch(`${baseUrl}${path}`, {
          method,
          headers: { 'content-type': 'application/json' },
          body: body === undefined ? undefined : JSON.stringify(body),
        });
        return { status: res.status, body: await res.json().catch(() => ({})) };
      };

      // ── 1. Author provider → endpoint → offering into the system config ──────
      const prov = await cfg('PUT', '/v1/config/providers/anthropic', {
        id: 'anthropic',
        slug: 'anthropic',
        display_name: 'Anthropic',
        version: 1,
      });
      assert.equal(prov.status, 200, 'provider stored');

      const ep = await cfg('PUT', '/v1/config/endpoints/ep-kimi', {
        id: 'ep-kimi',
        provider_id: 'anthropic',
        flavor: 'anthropic_messages',
        base_url: BASE,
        timeout_secs: 300,
        display_name: 'kimi',
        version: 1,
      });
      assert.equal(ep.status, 200, 'endpoint stored');

      const off = await cfg('POST', '/v1/config/offerings', {
        model_id: MODEL,
        provider_id: 'anthropic',
        protocol_endpoint_id: 'ep-kimi',
        flavor: 'anthropic_messages',
        upstream_model: null,
      });
      assert.equal(off.status, 200, 'offering stored');
      pass(`authored provider + endpoint + offering for ${MODEL} @ ${BASE}`);

      const catalog = await cfg('GET', '/v1/config/catalog');
      assert.equal(catalog.status, 200);
      assert.ok(JSON.stringify(catalog.body).includes(MODEL), 'catalog carries the offering');
      pass('GET /v1/config/catalog reflects the authored offering');

      // ── 2. Enter the credential (secret crosses the wire once, write-only) ───
      const created = await cfg('POST', '/v1/config/credentials', {
        workspace_id: WS,
        kind: 'vault',
        provider_id: 'anthropic',
        env_key: 'ANTHROPIC_API_KEY',
        secret: KEY,
      });
      assert.equal(created.status, 201, 'credential created');
      const credId = created.body.id;
      assert.ok(credId, 'credential id returned');
      assert.ok(!JSON.stringify(created.body).includes(KEY), 'the secret is never echoed on create');
      pass(`credential entered into the system config: ${credId} (secret-free response)`);

      const gotCred = await cfg('GET', `/v1/config/credentials/${credId}`);
      assert.equal(gotCred.status, 200);
      assert.ok(!JSON.stringify(gotCred.body).includes(KEY), 'GET credential is secret-free');
      pass('GET credential row is secret-free');

      // ── 3. Resolve the binding (secret-free triple) ─────────────────────────
      const resolved = await cfg('POST', '/v1/config/inference/resolve', {
        workspace_id: WS,
        model_id: MODEL,
        binding: { type: 'exact', credential_source_id: credId },
      });
      assert.equal(resolved.status, 200, `resolve ok (got ${resolved.status}: ${JSON.stringify(resolved.body)})`);
      assert.equal(resolved.body.adapter_kind, 'anthropic', 'resolves to the anthropic adapter');
      assert.equal(resolved.body.base_url, BASE, 'resolves to the authored endpoint base_url');
      assert.equal(resolved.body.model_id, MODEL);
      assert.equal(resolved.body.credential_present, true, 'a credential materialized behind the resolve');
      assert.ok(!JSON.stringify(resolved.body).includes(KEY), 'the resolve view is secret-free');
      pass('POST /v1/config/inference/resolve -> secret-free triple (adapter=anthropic, base_url, credential_present)');

      // ── 4. LIVE validate: probe the real KIMI model with the entered secret ──
      const validated = await cfg('POST', `/v1/config/credentials/${credId}/validate`, {
        workspace_id: WS,
        model_id: MODEL,
      });
      assert.equal(validated.status, 200, `validate ok (got ${validated.status}: ${JSON.stringify(validated.body)})`);
      assert.equal(validated.body.adapter_kind, 'anthropic');
      assert.equal(
        validated.body.status,
        'valid',
        `the HTTP-entered credential live-validates against ${MODEL} (got ${JSON.stringify(validated.body)})`,
      );
      pass(`LIVE probe: the system-config credential validates against the real model (${MODEL})`);
    });

    console.log('E2E PASS: config-plane author -> resolve -> LIVE validate against a real model, no env-var model config.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
