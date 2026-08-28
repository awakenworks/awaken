// Real-model token-aware compaction e2e. Drives a session through the official
// Anthropic TS SDK against awaken-server in `compaction` mode backed by a
// REAL Anthropic-compatible model (KIMI via AWAKEN_MODEL_SOURCE=http), with a
// small *configured* context window (AWAKEN_COMPACT_MAX_TOKENS) so a handful of
// large turns crosses the frozen effective window and the compactor sub-agent — itself a
// real model run — folds the older slice. Proves the token-aware trigger fires
// against a live LLM and surfaces `agent.thread_context_compacted` on the wire.
//
// Run: KIMI_API_KEY=... KIMI_BASE_URL=... KIMI_MODEL=... node managed_compaction_real_e2e.mjs
//   (ANTHROPIC_* aliases also work). Self-skips when no key is set.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  pass,
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38233);
const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  if (!process.env.KIMI_API_KEY && !process.env.ANTHROPIC_API_KEY) {
    console.log('SKIP managed_compaction_real_e2e: no KIMI_API_KEY / ANTHROPIC_API_KEY set.');
    return;
  }
  // A small configured window: fold once the transcript reaches 0.5 * 1000 = 500
  // estimated tokens, keeping the last 2 messages verbatim.
  const { server, baseUrl } = spawnServer('compaction', PORT, {
    AWAKEN_MODEL_SOURCE: 'http',
    AWAKEN_COMPACT_MAX_TOKENS: '500',
    AWAKEN_COMPACT_KEEP_LAST: '2',
  });
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

    // Each turn carries a large message; after a few, the committed transcript
    // exceeds the 500-token budget and the older slice folds.
    const big = (n) => `Message ${n}. ` + 'Please keep this context in mind. '.repeat(40);
    let compacted = false;
    let turns = 0;
    let history = [];
    // Turn decision K1: C1 exact per-turn receipt and C2 real reply; C3 the
    // cumulative configured budget crosses threshold. E1 receipt-scoped reply;
    // E2 compaction event. Constraint: older replies cannot satisfy C2 for a
    // later receipt. D1=C1+C2=>E1; D2=D1+C3=>E2.
    for (let i = 1; i <= 6 && !compacted; i += 1) {
      turns = i;
      const receipt = (await client.beta.sessions.events.send(s.id, {
        betas: BETAS,
        events: [{ type: 'user.message', content: [{ type: 'text', text: big(i) }] }],
      })).data[0];
      ({ events: history } = await waitForSessionEventReceipt(
        client,
        s.id,
        receipt.id,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message'),
        `real-model compaction turn ${i}`,
        { timeoutMs: 180_000 },
      ));
      const types = history.map((e) => e.type);
      assert.ok(types.includes('agent.message'), `turn ${i}: the real model replied — ${types.join(',')}`);
      compacted = types.includes('agent.thread_context_compacted');
    }
    assert.ok(compacted, `token-aware compaction fired against the real model within ${turns} turns`);
    pass(`real-model token-aware compaction fired after ${turns} turns`);

    // The projected event carries the SDK shape.
    const ev = history.find((e) => e.type === 'agent.thread_context_compacted');
    assert.equal(typeof ev.id, 'string');
    assert.equal(typeof ev.processed_at, 'string');
    pass('agent.thread_context_compacted has the SDK shape (id + processed_at)');

    console.log('E2E PASS: real-model token-aware compaction at the frozen effective window.');
  } finally {
    await stopServer(server);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
