// Token-aware compaction end-to-end (deterministic model). Proves the new
// `max_tokens` × `trigger_ratio` trigger fires through the *real server wire*
// (HTTP + official SDK + env config → token fold → compactor sub-run → event),
// independent of a live LLM: the server runs in `compaction` mode with the
// deterministic model but a *token* budget (AWAKEN_COMPACT_MAX_TOKENS), so a
// couple of large turns cross 0.5 × 200 = 100 est. tokens and the older slice
// folds — something the default message threshold (40) would never do this fast,
// which is exactly what distinguishes token mode from message mode.
//
// The live-LLM twin is `managed_compaction_real_e2e.mjs` (KIMI/Anthropic keys).
//
// Run: (from e2e/)  node managed_compaction_tokens_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38234);
const BETAS = ['managed-agents-2026-04-01'];

async function allEvents(client, id) {
  const evs = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) evs.push(ev);
  return evs;
}

async function main() {
  const { server, baseUrl } = spawnServer('compaction', PORT, {
    AWAKEN_COMPACT_MAX_TOKENS: '200', // budget = 0.5 * 200 = 100 est. tokens
    AWAKEN_COMPACT_TRIGGER_RATIO: '0.5',
    AWAKEN_COMPACT_KEEP_LAST: '1',
  });
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const s = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });

    // Large turns (~200 est. tokens each) push the committed transcript past the
    // 100-token budget within a couple of turns — far below the 40-message
    // default threshold, so a fold here can only be token-triggered.
    const big = (n) => `Turn ${n}: ` + 'filler '.repeat(120);
    let compacted = false;
    let turns = 0;
    for (let i = 1; i <= 4 && !compacted; i += 1) {
      turns = i;
      await client.beta.sessions.events.send(s.id, {
        betas: BETAS,
        events: [{ type: 'user.message', content: [{ type: 'text', text: big(i) }] }],
      });
      const types = (await allEvents(client, s.id)).map((e) => e.type);
      assert.ok(types.includes('agent.message'), `turn ${i} replied: ${types.join(',')}`);
      compacted = types.includes('agent.thread_context_compacted');
    }
    assert.ok(compacted, `the token budget folded within ${turns} turns (message threshold would not)`);
    assert.ok(turns <= 3, `folded early (${turns} turns) — token-triggered, not message-count`);
    pass(`token-aware compaction fired after ${turns} large turns`);

    const ev = (await allEvents(client, s.id)).find((e) => e.type === 'agent.thread_context_compacted');
    assert.equal(typeof ev.id, 'string');
    assert.equal(typeof ev.processed_at, 'string');
    pass('agent.thread_context_compacted has the SDK shape');

    console.log('E2E PASS: token-aware compaction (fold at a fraction of max_tokens) via the server wire.');
  } finally {
    await stopServer(server);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
