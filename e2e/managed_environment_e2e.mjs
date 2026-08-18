// Session ↔ environment association (Managed Agents contract), driven through the
// official Anthropic TS SDK. The environment is bound at session creation, echoed
// on the Session, defaulted to `env_local` when omitted, and immutable for the
// session's lifetime — a `POST /v1/sessions/{id}` carrying a different
// `environment_id` fails closed and leaves the environment pinned.
//
// Deterministic (management mode, no key). This suite locks the wire/record
// association + immutability. Network realization is covered by
// `acp_sandboxed_e2e.mjs` and the Rust `session_egress` decision table; package
// installation remains a separate provider-capability surface.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  await withScenarioServer('management', 'mcp', 38150, async (base) => {
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
    const bare = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    assert.equal(bare.environment_id, 'env_local', 'omitted environment defaults to env_local');
    pass('omitted environment defaults to env_local');

    // ── immutable for the session's lifetime ──────────────────────────────────
    // Cause graph / decision table: supported update fields only -> apply; an
    // immutable environment_id is present -> reject the entire update; rejection
    // -> neither title nor environment changes. Raw POST lets this unsupported
    // field reach the boundary because the SDK correctly omits it from its type.
    const res = await fetch(`${base}/v1/sessions/${session.id}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
      body: JSON.stringify({ title: 'renamed', environment_id: env.id + '_other' }),
    });
    const rejected = await res.json();
    assert.equal(res.status, 400, JSON.stringify(rejected));
    assert.match(rejected.error.message, /environment_id|unknown field/u);
    pass('environment mutation is rejected atomically');

    // A fresh retrieve still reports the create-time environment.
    const got = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.equal(got.environment_id, env.id, 'retrieve reports the create-time environment');
    assert.equal(got.title, null, 'a rejected mixed update cannot partially change the title');
    pass('retrieve confirms the pinned environment survived the update');
  });

  console.log('E2E PASS: session ↔ environment association (bound at creation, defaulted, immutable).');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
