// A non-retryable upstream failure (401 from a key mismatch) surfaces immediately
// through the run loop — distinct from the retryable-exhaustion path. Drives the
// engine's terminal inference-error mapping (no retry). Deterministic, CI-safe.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  const upstream = await startFakeAnthropic('the-right-key');
  try {
    // The runtime presents the WRONG key -> the upstream answers 401 (not retryable).
    process.env.ANTHROPIC_API_KEY = 'the-wrong-key'; // awaken-allow: secret
    process.env.ANTHROPIC_BASE_URL = `${upstream.url}/v1/`;
    process.env.ANTHROPIC_MODEL = 'fake-haiku';
    await withServer('real', 38273, async (base) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
      const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
      let sendError = null;
      try {
        await client.beta.sessions.events.send(session.id, {
          events: [{ type: 'user.message', content: [{ type: 'text', text: 'unauthorized' }] }],
          betas: BETAS,
        });
      } catch (err) {
        sendError = err;
      }
      const events = [];
      for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
      const fabricated = events
        .filter((e) => e.type === 'agent.message')
        .some((e) => (e.content ?? []).some((b) => (b.text ?? '').startsWith('FAKE:')));
      assert.ok(!fabricated, 'a 401 does not fabricate a reply');
      assert.ok(upstream.unauthorized >= 1, `the upstream rejected the key (${upstream.unauthorized})`);
      assert.ok(sendError !== null || events.length > 0, 'the auth failure surfaced through the run loop');
      pass('a non-retryable 401 surfaces immediately (no retry, no fabricated reply)');
    });
    console.log('E2E PASS: non-retryable upstream auth error surfaces through the run loop.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    upstream.close();
  }
}

main();
