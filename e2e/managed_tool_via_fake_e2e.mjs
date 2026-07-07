// Streamed tool round-trip over the REAL provider path WITHOUT a live key: the
// fake upstream asks for a tool (`use-tool:glob`), the runtime executes it and
// feeds the tool_result back, and the follow-up call answers. Drives the engine's
// streaming + tool-execution + multi-step path, then a plain follow-up turn.
// Deterministic, CI-safe.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-fake-tool-key'; // awaken-allow: secret

async function types(client, id) {
  const t = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) t.push(ev.type);
  return t;
}

async function send(client, id, text) {
  await client.beta.sessions.events.send(id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

async function main() {
  const upstream = await startFakeAnthropic(FAKE_KEY);
  try {
    process.env.ANTHROPIC_API_KEY = FAKE_KEY;
    process.env.ANTHROPIC_BASE_URL = `${upstream.url}/v1/`;
    process.env.ANTHROPIC_MODEL = 'fake-haiku';
    await withServer('real', 38261, async (base) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
      const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });

      // Turn 1: streamed tool round-trip.
      await send(client, session.id, 'use-tool:glob');
      const first = await types(client, session.id);
      assert.ok(first.includes('agent.tool_use'), `tool_use emitted: ${first}`);
      assert.ok(first.includes('agent.tool_result'), `tool_result emitted: ${first}`);
      assert.ok(first.includes('agent.message'), `a final message followed the tool: ${first}`);
      assert.ok(upstream.requests.length >= 2, `the tool round-trip made two model calls (${upstream.requests.length})`);
      assert.ok(upstream.requests.every((r) => r.stream), 'the real path streamed from the provider');
      pass('streamed tool round-trip: tool_use -> execute -> tool_result -> reply');

      // Turn 2: a plain follow-up on the same session.
      await send(client, session.id, 'just talk now');
      const second = await types(client, session.id);
      const replies = second.filter((t) => t === 'agent.message').length;
      assert.ok(replies >= 2, `multi-turn accumulates replies (${replies})`);
      pass('multi-turn continues over the streamed real path');
    });
    console.log('E2E PASS: streamed tool round-trip + multi-turn over the real provider path.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    upstream.close();
  }
}

main();
