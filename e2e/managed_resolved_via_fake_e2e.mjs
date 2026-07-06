// Resolver-backed provider path WITHOUT a live key: drives `real-resolved` mode
// (executor built through resolve_inference + executor_from_resolved) against a
// fake Anthropic upstream. Exercises the config → resolve → run path end to end
// (model-catalog + credential + config-resolver + genai over the wire) that the
// key-gated managed_resolved_real e2e otherwise skips. Deterministic, CI-safe.
//
// Run: (from e2e/)  node managed_resolved_via_fake_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-fake-resolved-key'; // awaken-allow: secret

async function main() {
  const upstream = await startFakeAnthropic(FAKE_KEY);
  try {
    process.env.ANTHROPIC_API_KEY = FAKE_KEY;
    process.env.ANTHROPIC_BASE_URL = `${upstream.url}/v1/`;
    process.env.ANTHROPIC_MODEL = 'fake-haiku';
    await withServer('real-resolved', 38241, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      assert.ok(session.id.startsWith('sesn_'));
      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'resolve me' }] }],
        betas: BETAS,
      });
      const events = [];
      for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
      const msg = events.find((e) => e.type === 'agent.message');
      assert.ok(msg, `expected an agent.message in ${events.map((e) => e.type)}`);
      const text = (msg.content ?? []).map((c) => c.text ?? '').join('');
      assert.ok(text.includes('FAKE:resolve me'), `the resolver-backed executor hit the wire: ${text}`);
      assert.ok(upstream.requests.length >= 1, 'the fake upstream received the resolved inference call');
      pass('config → resolve → run drove the resolver-backed executor over the wire');
    });
    console.log('E2E PASS: resolver-backed provider path over a fake upstream (config → resolve → run).');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    upstream.close();
  }
}

main();
