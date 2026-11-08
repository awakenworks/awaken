// Real-model Gemini e2e via OAuth (ADR-0043 Phase 3). Drives a Managed Agents
// session through the official Anthropic TypeScript SDK against awaken-server
// in `real` mode, bound through the config-plane API to **Gemini on Vertex AI**
// authenticated by a Google OAuth2 Bearer token: the provider connection names
// the server-owned `gcloud` OAuth helper, which mints the token. This proves the
// OAuth + Gemini flavor end to end through the managed adapter and the official SDK.
//
// Run: (from e2e/, with gcloud logged in; the harness passes these to the
// server only through the config-plane API)
//   GEMINI_PROJECT=my-proj GEMINI_LOCATION=global GEMINI_MODEL=gemini-2.5-flash \
//   node managed_real_gemini_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { modelUpstreamEnv, pass, waitForSessionEventReceipt, withServer } from './harness.mjs';
import { liveVertexUpstream } from './model_upstream.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  const live = liveVertexUpstream();
  if (!live) {
    console.log('SKIP managed_real_gemini_e2e: set GEMINI_PROJECT (and be logged into gcloud).');
    return;
  }
  try {
    await withServer('real', Number(process.env.E2E_PORT), async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      pass(`session created against Gemini/Vertex: ${session.id}`);

      // Test design: C1 exact SDK command receipt and C2 Vertex reply; E1 the
      // receipt is processed with a nonempty agent.message. K1 old history is
      // excluded by the receipt delta. D1=C1+C2=>E1.
      const receipt = (await client.beta.sessions.events.send(session.id, {
        events: [
          { type: 'user.message', content: [{ type: 'text', text: 'Reply with exactly the single word: pong' }] },
        ],
        betas: BETAS,
      })).data[0];
      const { delta: events } = await waitForSessionEventReceipt(
        client,
        session.id,
        receipt.id,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message'),
        'Gemini/Vertex Managed reply',
        { timeoutMs: 180_000 },
      );
      const types = events.map((e) => e.type);
      assert.ok(types.includes('agent.message'), `expected agent.message, got ${types}`);
      const msg = events.find((e) => e.type === 'agent.message');
      const text = (msg.content ?? []).map((c) => c.text ?? '').join('').trim();
      assert.ok(text.length > 0, 'Gemini returned non-empty text');
      pass(`Gemini replied via managed session (OAuth): ${JSON.stringify(text.slice(0, 80))}`);
    }, modelUpstreamEnv(null, live));

    console.log('E2E PASS: Gemini-on-Vertex via OAuth drives a real managed session through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
