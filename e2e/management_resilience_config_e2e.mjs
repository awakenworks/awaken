// Provider-resilience config plane (E3-2 / E3-4), end-to-end over HTTP. CI-safe:
// no live model — offerings point at a dummy base_url and we only drive resolution
// and the credential-availability ops surface.
//
//   • POST /v1/config/inference-profiles/:id/resolve-candidates
//       -> the ordered (model × credential) candidate list a profile's AxisBinding
//          (pin/pool) resolves to  [AxisBinding / model_axis / resolve_profile_candidates]
//   • POST /v1/config/credentials/:id/cooldown  {kind, retry_after_secs}
//       -> records a Disposition-driven cooldown  [Disposition / cooldown_deadline / cool_down]
//   • GET  /v1/config/credentials/:id/availability   -> AvailabilityState
//   • GET  /v1/config/credential-pools/:id/eligible  -> eligible_order (cooled dropped)

import assert from 'node:assert/strict';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38472);
const WS = 'ws';

async function main() {
  await withServer('management', PORT, async (baseUrl) => {
    const cfg = async (method, path, body) => {
      const res = await fetch(`${baseUrl}${path}`, {
        method,
        headers: { 'content-type': 'application/json' },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
      return { status: res.status, body: await res.json().catch(() => ({})) };
    };
    const ok = (r, s, m) => assert.equal(r.status, s, `${m} (got ${r.status}: ${JSON.stringify(r.body)})`);

    // ── Catalog: one provider, one endpoint, TWO offerings (a model pool) ────
    ok(await cfg('PUT', '/v1/config/providers/anthropic', { id: 'anthropic', slug: 'anthropic', display_name: 'A', version: 1 }), 200, 'provider');
    ok(await cfg('PUT', '/v1/config/endpoints/ep1', { id: 'ep1', provider_id: 'anthropic', flavor: 'anthropic_messages', base_url: 'https://example.invalid/v1/', timeout_secs: 300, display_name: 'd', version: 1 }), 200, 'endpoint');
    for (const m of ['model-a', 'model-b']) {
      ok(await cfg('POST', '/v1/config/offerings', { model_id: m, provider_id: 'anthropic', protocol_endpoint_id: 'ep1', flavor: 'anthropic_messages', upstream_model: null }), 200, `offering ${m}`);
    }

    // ── Two anthropic-scoped credentials + a pool over them ─────────────────
    const c1 = await cfg('POST', '/v1/config/credentials', { workspace_id: WS, kind: 'vault', provider_id: 'anthropic', env_key: 'K1', secret: 'sk-a' });
    ok(c1, 201, 'cred1');
    const c2 = await cfg('POST', '/v1/config/credentials', { workspace_id: WS, kind: 'vault', provider_id: 'anthropic', env_key: 'K2', secret: 'sk-b' });
    ok(c2, 201, 'cred2');
    const id1 = c1.body.id, id2 = c2.body.id;
    ok(await cfg('PUT', '/v1/config/credential-pools/pool1', {
      id: 'pool1',
      workspace_id: WS,
      members: [
        { credential_source_id: id1, ordinal: 0, enabled: true, selection_weight: 0 },
        { credential_source_id: id2, ordinal: 1, enabled: true, selection_weight: 0 },
      ],
    }), 200, 'pool');
    pass('authored a 2-model catalog + a 2-member credential pool');

    // ── A profile: model-a pinned, model-b as a fallback; pool credential ────
    ok(await cfg('PUT', '/v1/config/inference-profiles/prof1', {
      model_id: 'model-a',
      model_fallbacks: ['model-b'],
      credential_binding: { type: 'one_of_credential_pool', credential_pool_id: 'pool1' },
      disabled_endpoint_ids: [],
    }), 200, 'profile');

    // resolve-candidates returns BOTH models in axis order, each with a credential.
    const cand = await cfg('POST', '/v1/config/inference-profiles/prof1/resolve-candidates', { workspace_id: WS });
    ok(cand, 200, 'resolve-candidates');
    assert.equal(cand.body.candidates.length, 2, `two candidates (got ${JSON.stringify(cand.body)})`);
    assert.deepEqual(cand.body.candidates.map((c) => c.model_id), ['model-a', 'model-b'], 'axis order preserved');
    assert.ok(cand.body.candidates.every((c) => c.credential_present), 'each candidate materialized a pool credential');
    pass('resolve-candidates -> ordered (model-a, model-b) candidate list (AxisBinding pool)');

    // ── Availability + cooldown ops surface ─────────────────────────────────
    let avail = await cfg('GET', `/v1/config/credentials/${id1}/availability`);
    ok(avail, 200, 'availability');
    assert.equal(avail.body.state, 'available', 'a fresh source is available');

    let elig = await cfg('GET', '/v1/config/credential-pools/pool1/eligible');
    ok(elig, 200, 'eligible');
    assert.deepEqual(elig.body.eligible, [id1, id2], 'both members eligible before any cooldown');
    assert.deepEqual(elig.body.cooled, [], 'none cooled');

    // Cool member 1 with a quota signal (Disposition::Quota -> cooldown_deadline).
    const cooled = await cfg('POST', `/v1/config/credentials/${id1}/cooldown`, { kind: 'quota', retry_after_secs: 3600 });
    ok(cooled, 200, 'cooldown');
    assert.equal(cooled.body.state, 'cooled_down', `member 1 is cooled (got ${JSON.stringify(cooled.body)})`);
    assert.ok(typeof cooled.body.retry_at_ms === 'number', 'a retry_at deadline is set');

    // The pool now rotates past the cooled member.
    elig = await cfg('GET', '/v1/config/credential-pools/pool1/eligible');
    assert.deepEqual(elig.body.eligible, [id2], 'cooled member rotated out of the eligible order');
    assert.deepEqual(elig.body.cooled, [id1], 'member 1 reported cooled');
    pass('quota cooldown rotates the pool: eligible_order drops the cooled member');

    // Clear lifts the cooldown; the member is available again.
    ok(await cfg('POST', `/v1/config/credentials/${id1}/cooldown`, { kind: 'available' }), 200, 'clear');
    elig = await cfg('GET', '/v1/config/credential-pools/pool1/eligible');
    assert.deepEqual(elig.body.eligible, [id1, id2], 'both eligible again after clear');

    // Hard exhaustion stays until cleared; transient/permanent are availability no-ops.
    const exh = await cfg('POST', `/v1/config/credentials/${id2}/cooldown`, { kind: 'exhausted' });
    assert.equal(exh.body.state, 'exhausted', 'exhausted state recorded');
    const noop = await cfg('POST', `/v1/config/credentials/${id1}/cooldown`, { kind: 'transient' });
    assert.equal(noop.body.state, 'available', 'a transient failure does not cool the identity');
    const perm = await cfg('POST', `/v1/config/credentials/${id1}/cooldown`, { kind: 'permanent' });
    assert.equal(perm.body.state, 'available', 'a permanent failure is a next-binding decision, not a cooldown');
    pass('availability states: quota/exhausted cool, clear lifts, transient/permanent are no-ops');

    // ── A pool whose only member has no materializable source → fail-closed ──
    ok(await cfg('PUT', '/v1/config/credential-pools/deadpool', {
      id: 'deadpool',
      workspace_id: WS,
      members: [{ credential_source_id: 'ghost-source', ordinal: 0, enabled: true, selection_weight: 0 }],
    }), 200, 'dead pool');
    const dead = await cfg('POST', '/v1/config/inference/resolve', {
      workspace_id: WS,
      model_id: 'model-a',
      binding: { type: 'one_of_credential_pool', credential_pool_id: 'deadpool' },
    });
    assert.equal(dead.status, 409, `an all-unmaterializable pool is fail-closed 409 (got ${dead.status}: ${JSON.stringify(dead.body)})`);
    assert.ok(JSON.stringify(dead.body).includes('pool_exhausted'), 'the problem body names pool_exhausted (NoEligibleCredential)');
    pass('resolve(pool with no usable member) -> 409 pool_exhausted (NoEligibleCredential)');

    console.log('E2E PASS: provider-resilience config plane — axis candidates + credential availability cooldown/rotation.');
  });
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
