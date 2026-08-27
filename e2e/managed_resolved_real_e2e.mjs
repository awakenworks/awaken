// Resolver-backed real-model e2e (ADR-0043): drives a Managed Agents session
// through the official Anthropic TypeScript SDK against awaken-server in
// `real-resolved` mode, whose executor is built **through the resolver**
// (`resolve_inference` + `executor_from_materialized_endpoint`) rather than directly. This
// exercises the config → resolve → run path — model-catalog + credential +
// config-resolver — end to end with a real model return via the TS SDK.
//
// Run: (from e2e/, with a live key)
//   ANTHROPIC_API_KEY=... ANTHROPIC_BASE_URL=... ANTHROPIC_MODEL=... node managed_resolved_real_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { pass, waitForSessionEventReceipt, withServer } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  if (!process.env.ANTHROPIC_API_KEY && !process.env.KIMI_API_KEY) {
    console.log('SKIP managed_resolved_real_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  try {
    await withServer('real-resolved', 38151, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      pass(`session created (resolver-backed): ${session.id}`);

      // Test design: C1 resolver-backed exact command receipt and C2 live reply;
      // E1 processed receipt with agent.message. K1 pre-command history cannot
      // satisfy the resolver oracle. D1=C1+C2=>E1.
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
        'resolver-backed real model reply',
        { timeoutMs: 180_000 },
      );
      const msg = events.find((e) => e.type === 'agent.message');
      assert.ok(msg, `expected agent.message in ${events.map((e) => e.type)}`);
      const text = (msg.content ?? []).map((c) => c.text ?? '').join('').trim();
      assert.ok(text.length > 0, 'the resolver-backed real model returned non-empty text');
      pass(`real model replied via the resolver path: ${JSON.stringify(text.slice(0, 80))}`);
    });

    console.log('E2E PASS: config → resolve → run drives a real model through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
