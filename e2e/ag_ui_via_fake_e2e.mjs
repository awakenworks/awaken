// The AG-UI protocol adapter over the REAL provider path WITHOUT a live key: the
// official @ag-ui/client HttpAgent drives `real` mode against a fake Anthropic
// upstream, so the ag-ui request/router/encoder carry a real (wire) model turn —
// the path the key-gated real e2e skips. Deterministic, CI-safe.

import assert from 'node:assert/strict';
import { HttpAgent } from '@ag-ui/client';
import { withServer, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const FAKE_KEY = 'sk-fake-agui-key'; // awaken-allow: secret

async function main() {
  const upstream = await startFakeAnthropic(FAKE_KEY);
  try {
    process.env.ANTHROPIC_API_KEY = FAKE_KEY;
    process.env.ANTHROPIC_BASE_URL = `${upstream.url}/v1/`;
    process.env.ANTHROPIC_MODEL = 'fake-haiku';
    await withServer('real', 38246, async (base) => {
      const agent = new HttpAgent({ url: `${base}/v1/ag-ui/agents/assistant` });
      agent.messages = [{ id: 'u1', role: 'user', content: 'over ag-ui' }];
      const res = await agent.runAgent();
      const produced = res?.newMessages ?? [];
      const last = produced[produced.length - 1];
      const text =
        typeof last?.content === 'string'
          ? last.content
          : (last?.content ?? []).map((c) => c.text ?? '').join('');
      assert.ok(upstream.requests.length >= 1, 'the fake upstream received the ag-ui-driven inference call');
      assert.ok(text.includes('FAKE:over ag-ui'), `ag-ui carried the wire reply: ${text}`);
      pass('ag-ui adapter carried a real (wire) model turn end to end');
    });
    console.log('E2E PASS: ag-ui adapter drives the real provider path over a fake upstream.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    upstream.close();
  }
}

main();
