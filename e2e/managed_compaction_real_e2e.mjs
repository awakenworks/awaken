// Real-model token-aware compaction e2e. Drives a session through the official
// Anthropic TS SDK against awaken-server-local in `compaction` mode backed by a
// REAL Anthropic-compatible model (KIMI via AWAKEN_MODEL_SOURCE=http), with a
// small *configured* context window (AWAKEN_COMPACT_MAX_TOKENS) so a handful of
// large turns crosses the trigger ratio and the compactor sub-agent — itself a
// real model run — folds the older slice. Proves the token-aware trigger fires
// against a live LLM and surfaces `agent.thread_context_compacted` on the wire.
//
// Run: KIMI_API_KEY=... KIMI_BASE_URL=... KIMI_MODEL=... node managed_compaction_real_e2e.mjs
//   (ANTHROPIC_* aliases also work). Self-skips when no key is set.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38233);
const BETAS = ['managed-agents-2026-04-01'];

async function allEvents(client, id) {
  const evs = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) evs.push(ev);
  return evs;
}

async function main() {
  if (!process.env.KIMI_API_KEY && !process.env.ANTHROPIC_API_KEY) {
    console.log('SKIP managed_compaction_real_e2e: no KIMI_API_KEY / ANTHROPIC_API_KEY set.');
    return;
  }
  // A small configured window: fold once the transcript reaches 0.5 * 1000 = 500
  // estimated tokens, keeping the last 2 messages verbatim.
  const { server, baseUrl } = spawnServer('compaction', PORT, {
    AWAKEN_MODEL_SOURCE: 'http',
    AWAKEN_COMPACT_MAX_TOKENS: '1000',
    AWAKEN_COMPACT_TRIGGER_RATIO: '0.5',
    AWAKEN_COMPACT_KEEP_LAST: '2',
  });
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const s = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });

    // Each turn carries a large message; after a few, the committed transcript
    // exceeds the 500-token budget and the older slice folds.
    const big = (n) => `Message ${n}. ` + 'Please keep this context in mind. '.repeat(40);
    let compacted = false;
    let turns = 0;
    for (let i = 1; i <= 6 && !compacted; i += 1) {
      turns = i;
      await client.beta.sessions.events.send(s.id, {
        betas: BETAS,
        events: [{ type: 'user.message', content: [{ type: 'text', text: big(i) }] }],
      });
      const types = (await allEvents(client, s.id)).map((e) => e.type);
      assert.ok(types.includes('agent.message'), `turn ${i}: the real model replied — ${types.join(',')}`);
      compacted = types.includes('agent.thread_context_compacted');
    }
    assert.ok(compacted, `token-aware compaction fired against the real model within ${turns} turns`);
    pass(`real-model token-aware compaction fired after ${turns} turns`);

    // The projected event carries the SDK shape.
    const ev = (await allEvents(client, s.id)).find((e) => e.type === 'agent.thread_context_compacted');
    assert.equal(typeof ev.id, 'string');
    assert.equal(typeof ev.processed_at, 'string');
    pass('agent.thread_context_compacted has the SDK shape (id + processed_at)');

    console.log('E2E PASS: real-model token-aware compaction (fold at a fraction of max_tokens).');
  } finally {
    await stopServer(server);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
