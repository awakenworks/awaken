// The A2A protocol adapter over the REAL provider path WITHOUT a live key: the
// official @a2a-js/sdk A2AClient drives `real` mode against a fake Anthropic
// upstream, so the a2a router/types carry a real (wire) model turn — the path the
// key-gated real e2e skips. Deterministic, CI-safe.

import assert from 'node:assert/strict';
import { A2AClient } from '@a2a-js/sdk/client';
import { withServer, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const FAKE_KEY = 'sk-fake-a2a-key'; // awaken-allow: secret

async function main() {
  const upstream = await startFakeAnthropic(FAKE_KEY);
  try {
    process.env.ANTHROPIC_API_KEY = FAKE_KEY;
    process.env.ANTHROPIC_BASE_URL = `${upstream.url}/v1/`;
    process.env.ANTHROPIC_MODEL = 'fake-haiku';
    await withServer('real', 38247, async (base) => {
      const client = await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`);
      const r = await client.sendMessage({
        message: {
          messageId: 'm1',
          contextId: 'a2a-fake',
          role: 'user',
          kind: 'message',
          parts: [{ kind: 'text', text: 'over a2a' }],
        },
      });
      assert.ok(upstream.requests.length >= 1, 'the fake upstream received the a2a-driven inference call');
      assert.ok(JSON.stringify(r).includes('FAKE:over a2a'), `a2a carried the wire reply: ${JSON.stringify(r).slice(0, 200)}`);
      pass('a2a adapter carried a real (wire) model turn end to end');
    });
    console.log('E2E PASS: a2a adapter drives the real provider path over a fake upstream.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    upstream.close();
  }
}

main();
