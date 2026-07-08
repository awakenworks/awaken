// Managed session token-usage e2e: the `usage` field (BetaManagedAgentsSessionUsage)
// is populated from the real token counts the runtime records per inference and
// accumulates across turns. Two arms:
//   1) deterministic — over the real provider wire (GenaiExecutor → fake upstream),
//      the fake reports a fixed per-inference usage, so we assert EXACT accumulated
//      counts after one and two turns.
//   2) live-KIMI — against the real model (native `real` mode with the ~/.bashrc KIMI
//      config), we assert the counts are real and plausible (> 0). Skipped without a key.
//
// Run: (from e2e/)  node managed_usage_e2e.mjs
//   live arm: ANTHROPIC_API_KEY=sk-kimi-… ANTHROPIC_BASE_URL=https://api.kimi.com/coding/v1/ \
//             ANTHROPIC_MODEL=kimi-k2-0711-preview node managed_usage_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, withServer, pass } from './harness.mjs';
import { FAKE_USAGE } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function turn(client, id, text) {
  await client.beta.sessions.events.send(id, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
}
const usageOf = async (client, id) => (await client.beta.sessions.retrieve(id, { betas: BETAS })).usage ?? {};

async function main() {
  // ---- arm 1: deterministic exact usage over the real provider wire ----
  await withRealServer('echo', 38240, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });

    // A fresh session, no turn yet → no usage.
    const fresh = await usageOf(client, session.id);
    assert.ok(!fresh.input_tokens && !fresh.output_tokens, `fresh session has no usage: ${JSON.stringify(fresh)}`);
    pass('fresh session reports empty usage');

    // One turn → exactly one inference's worth of tokens.
    await turn(client, session.id, 'first');
    let u = await usageOf(client, session.id);
    assert.equal(u.input_tokens, FAKE_USAGE.input_tokens, `1 turn input_tokens: ${JSON.stringify(u)}`);
    assert.equal(u.output_tokens, FAKE_USAGE.output_tokens, `1 turn output_tokens: ${JSON.stringify(u)}`);
    pass(`session.usage after 1 turn = ${JSON.stringify(u)} (exact)`);

    // Second turn → the counts accumulate across turns.
    await turn(client, session.id, 'second');
    u = await usageOf(client, session.id);
    assert.equal(u.input_tokens, FAKE_USAGE.input_tokens * 2, `2 turns input_tokens accumulate: ${JSON.stringify(u)}`);
    assert.equal(u.output_tokens, FAKE_USAGE.output_tokens * 2, `2 turns output_tokens accumulate: ${JSON.stringify(u)}`);
    pass(`session.usage accumulates across turns = ${JSON.stringify(u)}`);
  });

  // ---- arm 2: live KIMI — real, plausible counts ----
  if (process.env.ANTHROPIC_API_KEY || process.env.KIMI_API_KEY) {
    await withServer('real', 38241, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
      await turn(client, session.id, 'Reply with exactly the single word: pong');
      const u = await usageOf(client, session.id);
      assert.ok(u.input_tokens > 0, `live model reported real input tokens: ${JSON.stringify(u)}`);
      assert.ok(u.output_tokens > 0, `live model reported real output tokens: ${JSON.stringify(u)}`);
      pass(`LIVE KIMI real usage = ${JSON.stringify(u)}`);
    });
  } else {
    console.log('SKIP live-KIMI usage arm: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
  }

  console.log('E2E PASS: managed session token usage (deterministic exact + live-KIMI real).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
