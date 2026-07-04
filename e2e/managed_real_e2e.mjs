// Real-model Managed Agents e2e: drive a session through the official Anthropic
// TypeScript SDK against awaken-server-local in `real` mode, which is backed by a
// live Anthropic-compatible model (GenaiExecutor) configured from the environment
// (ANTHROPIC_API_KEY/BASE_URL/MODEL or the KIMI_* aliases). This exercises the full
// managed session path — create → user.message → agent.message → status_idle —
// with a real model return, not the deterministic echo stub.
//
// Run: (from e2e/, with a live key)
//   ANTHROPIC_API_KEY=... ANTHROPIC_BASE_URL=... ANTHROPIC_MODEL=... node managed_real_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  if (!process.env.ANTHROPIC_API_KEY && !process.env.KIMI_API_KEY) {
    console.log('SKIP managed_real_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  try {
    await withServer('real', 38131, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      assert.equal(session.type, 'session');
      pass(`session created: ${session.id}`);

      await client.beta.sessions.events.send(session.id, {
        events: [
          { type: 'user.message', content: [{ type: 'text', text: 'Reply with exactly the single word: pong' }] },
        ],
        betas: BETAS,
      });

      const events = [];
      for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
      const types = events.map((e) => e.type);
      assert.ok(types.includes('agent.message'), `expected agent.message, got ${types}`);
      assert.ok(types.includes('session.status_idle'), `expected status_idle, got ${types}`);

      const msg = events.find((e) => e.type === 'agent.message');
      const text = (msg.content ?? []).map((c) => c.text ?? '').join('').trim();
      assert.ok(text.length > 0, 'the real model returned non-empty text');
      pass(`real model replied via managed session: ${JSON.stringify(text.slice(0, 80))}`);
    });

    console.log('E2E PASS: managed session drives a real model return through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
