// Cross-protocol token-usage agreement e2e (scenario #7): token accounting is
// committed thread state (runtime `ThreadUsage`), and every wire projects the SAME
// seam. `managed_usage_e2e` pins the Managed `session.usage` numbers; this test pins
// the AI-SDK `finish` projection to the SAME per-turn accounting, over ONE process,
// so the two wires provably agree on the runtime seam (`rt.usage`).
//
// Chain:
//   AI-SDK : POST /v1/ai-sdk/threads/T/runs -> ProtocolHost -> runtime commit ->
//            finish.messageMetadata.totalUsage = rt.usage(T)   (attach_usage)
//   Managed: sessions.events.send -> ManagedHost -> runtime commit ->
//            sessions.retrieve().usage                         (same ThreadUsage seam)
//
// Real provider path (fake Anthropic upstream) so usage is populated; the fake
// reports a fixed FAKE_USAGE per inference. Run: (from e2e/) node cross_protocol_usage_e2e.mjs

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';
import { FAKE_USAGE } from './fixtures/fake_anthropic_fixture.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38602);
const BETAS = ['managed-agents-2026-04-01'];

// The provider adapter normalizes input_tokens to the TOTAL input incl. prompt-cache
// (raw + cache_read + cache_creation) — the same normalization managed_usage asserts.
const PER_TURN_INPUT =
  FAKE_USAGE.input_tokens + FAKE_USAGE.cache_read_input_tokens + FAKE_USAGE.cache_creation_input_tokens;
const PER_TURN_OUTPUT = FAKE_USAGE.output_tokens;

// Drive one AI-SDK turn and return the finish chunk's totalUsage.
async function aiSdkTurn(base, thread, text) {
  const res = await fetch(`${base}/v1/ai-sdk/threads/${thread}/runs`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      threadId: thread,
      messages: [{ id: `u-${randomBytes(3).toString('hex')}`, role: 'user', parts: [{ type: 'text', text }] }],
    }),
  });
  assert.equal(res.status, 200, `ai-sdk run accepted (${res.status})`);
  const raw = await res.text();
  let usage = null;
  for (const line of raw.split('\n')) {
    const t = line.trim();
    if (!t.startsWith('data:')) continue;
    const p = t.slice(5).trim();
    if (!p || p === '[DONE]') continue;
    let ev;
    try {
      ev = JSON.parse(p);
    } catch {
      continue;
    }
    if (ev.type === 'finish' && ev.messageMetadata?.totalUsage) usage = ev.messageMetadata.totalUsage;
  }
  assert.ok(usage, `ai-sdk finish carried messageMetadata.totalUsage: ${raw.slice(0, 400)}`);
  return usage;
}

async function main() {
  await withRealServer('echo', PORT, async (base) => {
    // --- AI-SDK wire: exact usage on the finish chunk ----------------------
    const thread = `usage-${randomBytes(4).toString('hex')}`;
    let u = await aiSdkTurn(base, thread, 'first');
    assert.equal(u.inputTokens, PER_TURN_INPUT, `ai-sdk 1-turn inputTokens: ${JSON.stringify(u)}`);
    assert.equal(u.outputTokens, PER_TURN_OUTPUT, `ai-sdk 1-turn outputTokens: ${JSON.stringify(u)}`);
    assert.equal(u.totalTokens, PER_TURN_INPUT + PER_TURN_OUTPUT, `ai-sdk totalTokens = in+out`);
    pass(`AI-SDK finish.totalUsage after 1 turn = ${JSON.stringify(u)} (exact)`);

    // Second turn on the SAME thread accumulates (committed thread state).
    u = await aiSdkTurn(base, thread, 'second');
    assert.equal(u.inputTokens, PER_TURN_INPUT * 2, `ai-sdk 2-turn inputTokens accumulate: ${JSON.stringify(u)}`);
    assert.equal(u.outputTokens, PER_TURN_OUTPUT * 2, `ai-sdk 2-turn outputTokens accumulate: ${JSON.stringify(u)}`);
    pass('AI-SDK usage accumulates across turns on one committed thread');

    // --- Managed wire: the SAME per-turn accounting ------------------------
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
    await client.beta.sessions.events.send(session.id, {
      betas: BETAS,
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'first' }] }],
    });
    const m = (await client.beta.sessions.retrieve(session.id, { betas: BETAS })).usage ?? {};
    assert.equal(m.input_tokens, PER_TURN_INPUT, `managed session.usage input: ${JSON.stringify(m)}`);
    assert.equal(m.output_tokens, PER_TURN_OUTPUT, `managed session.usage output: ${JSON.stringify(m)}`);
    pass(`Managed session.usage after 1 turn = ${JSON.stringify(m)}`);

    // The invariant: both wires project the SAME runtime accounting seam.
    assert.equal(m.input_tokens, PER_TURN_INPUT, 'managed + ai-sdk agree on per-turn input accounting');
    assert.equal(m.output_tokens, PER_TURN_OUTPUT, 'managed + ai-sdk agree on per-turn output accounting');
    pass('Managed and AI-SDK project the SAME committed ThreadUsage seam (cross-protocol agreement)');
  });

  console.log('E2E PASS: cross-protocol token-usage agreement (AI-SDK finish == Managed session.usage seam).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
