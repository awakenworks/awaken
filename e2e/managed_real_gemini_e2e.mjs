// Real-model Gemini e2e via OAuth (ADR-0043 Phase 3). Drives a Managed Agents
// session through the official Anthropic TypeScript SDK against awaken-server
// in `real-gemini` mode, which is backed by **Gemini on Vertex AI** authenticated
// by a Google OAuth2 Bearer token. The server refreshes the token through the
// credential domain's OAuth helper (`GEMINI_ACCESS_TOKEN` or `gcloud auth
// print-access-token`). This proves the OAuth + Gemini flavor end to end through
// the managed adapter and the official SDK.
//
// Run: (from e2e/, with gcloud logged in)
//   GEMINI_PROJECT=my-proj GEMINI_LOCATION=global GEMINI_MODEL=gemini-2.5-flash \
//   node managed_real_gemini_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  if (!process.env.GEMINI_PROJECT) {
    console.log('SKIP managed_real_gemini_e2e: set GEMINI_PROJECT (and be logged into gcloud).');
    return;
  }
  try {
    await withServer('real-gemini', 38132, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      pass(`session created against Gemini/Vertex: ${session.id}`);

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
      const msg = events.find((e) => e.type === 'agent.message');
      const text = (msg.content ?? []).map((c) => c.text ?? '').join('').trim();
      assert.ok(text.length > 0, 'Gemini returned non-empty text');
      pass(`Gemini replied via managed session (OAuth): ${JSON.stringify(text.slice(0, 80))}`);
    });

    console.log('E2E PASS: Gemini-on-Vertex via OAuth drives a real managed session through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
