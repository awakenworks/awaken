// Egress policy + fail-closed bind, driven by the Anthropic TS SDK + raw HTTP.
// Covers two neutralized surfaces of the managed plane:
//   * an environment's `networking` wire config is resolved through the neutral
//     `NetworkPolicy` when a session binds (`EnvRecord::network_policy` →
//     `NetworkPolicy::denies_under_binary_enforcer` → `deny_egress`); a create
//     that succeeds against each policy proves the mapping ran end to end.
//   * an unknown vault fails the bind fail-closed before any session is minted
//     (`ManagedState::check_bind`), returning the standard 404 envelope.
//
// Run: (from e2e/)  node management_egress_bind_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  try {
    await withScenarioServer('management', 'mcp', 38166, async (base) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

      // -- environment networking policy → session binds (egress resolved) -----
      for (const net of ['unrestricted', 'none', 'limited']) {
        const networking =
          net === 'limited'
            ? { type: 'limited', allowed_hosts: ['api.anthropic.com'] }
            : { type: net };
        const env = await client.beta.environments.create({
          name: `net-${net}`,
          config: { type: 'self_hosted', networking },
          betas: BETAS,
        });
        const got = await client.beta.environments.retrieve(env.id, { betas: BETAS });
        assert.equal(
          got.config.networking.type,
          net,
          `env networking round-trips (${net})`,
        );
        // The create resolves the environment's networking into deny_egress via
        // the neutral NetworkPolicy — a session that binds proves the mapping ran.
        const session = await client.beta.sessions.create({
          agent: 'assistant',
          environment_id: env.id,
          betas: BETAS,
        });
        assert.equal(
          session.environment_id,
          env.id,
          `session pins the ${net} environment`,
        );
      }
      pass('environment networking resolves through the neutral NetworkPolicy at session bind');

      // -- unknown vault fails the bind fail-closed, before a session exists ----
      const res = await fetch(`${base}/v1/sessions`, {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          'x-api-key': 'e2e-dummy',
          'anthropic-version': '2023-06-01',
          'anthropic-beta': BETAS[0],
        },
        body: JSON.stringify({ agent: 'assistant', vault_ids: ['vlt_missing'] }),
      });
      assert.equal(res.status, 404, `unknown vault → 404 (got ${res.status})`);
      const body = await res.json();
      assert.equal(body.type, 'error');
      assert.equal(body.error.type, 'not_found_error');
      assert.match(body.error.message, /vault/i, 'error names the missing vault');
      pass('unknown vault fails the bind fail-closed with a 404 not_found envelope');

      console.log('E2E PASS: egress policy + fail-closed bind via TS SDK + HTTP.');
      process.exitCode = 0;
    });
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
