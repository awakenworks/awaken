// Session ↔ environment association (Managed Agents contract), driven through the
// official Anthropic TS SDK. The environment is bound at session creation, echoed
// on the Session, defaulted to `env_local` when omitted, and immutable for the
// session's lifetime — a `POST /v1/sessions/{id}` carrying a different
// `environment_id` updates only title/metadata and leaves the environment pinned.
//
// Deterministic (management mode, no key). Scope note: this locks the wire/record
// association + immutability, NOT sandbox realization — `environment_id` does not
// yet parameterize the local sandbox (networking/packages).

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  await withServer('management', 38150, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    // A real environment to reference.
    const env = await client.beta.environments.create({
      name: 'assoc-env',
      config: { type: 'self_hosted' },
      betas: BETAS,
    });
    assert.ok(env.id.startsWith('env_'), `environment created: ${env.id}`);

    // ── bound at creation: an explicit environment is echoed on the session ────
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: env.id,
      betas: BETAS,
    });
    assert.equal(session.environment_id, env.id, 'session pins the requested environment');
    pass(`session bound to environment at creation: ${env.id}`);

    // ── default when omitted ──────────────────────────────────────────────────
    const bare = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
    assert.equal(bare.environment_id, 'env_local', 'omitted environment defaults to env_local');
    pass('omitted environment defaults to env_local');

    // ── immutable for the session's lifetime ──────────────────────────────────
    // Update carries a different environment_id alongside a title change; only the
    // title takes effect. (Raw POST so the non-updatable field reaches the wire —
    // the SDK's typed update params do not expose environment_id.)
    const res = await fetch(`${base}/v1/sessions/${session.id}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
      body: JSON.stringify({ title: 'renamed', environment_id: env.id + '_other' }),
    });
    assert.equal(res.status, 200, `update returns 200 (got ${res.status})`);
    const updated = await res.json();
    assert.equal(updated.title, 'renamed', 'title is updatable');
    assert.equal(updated.environment_id, env.id, 'environment is immutable — update env ignored');
    pass('environment is immutable across update (only title changed)');

    // A fresh retrieve still reports the create-time environment.
    const got = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.equal(got.environment_id, env.id, 'retrieve reports the create-time environment');
    pass('retrieve confirms the pinned environment survived the update');
  });

  console.log('E2E PASS: session ↔ environment association (bound at creation, defaulted, immutable).');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
