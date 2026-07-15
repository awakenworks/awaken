// E2E: token-aware compaction whose window comes from the MODEL ATTRIBUTE, not a
// compaction override. `AWAKEN_COMPACT_MAX_TOKENS` is deliberately UNSET; only the
// model's published context window (`AWAKEN_MODEL_CONTEXT_WINDOW` — this harness's
// projection of the catalog's `ModelSpec.context_window`) is configured. So the fold
// firing proves `CompactConfig::effective_max_tokens` inherited the window from the
// model attribute end to end, against a live KIMI model.
//
// Run: KIMI_API_KEY=... node managed_compaction_model_window_e2e.mjs (self-skips w/o key).

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38261);
const BETAS = ['managed-agents-2026-04-01'];

async function allEvents(client, id) {
  const evs = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) evs.push(ev);
  return evs;
}

async function main() {
  if (!process.env.KIMI_API_KEY && !process.env.ANTHROPIC_API_KEY) {
    console.log('SKIP managed_compaction_model_window_e2e: no KIMI_API_KEY / ANTHROPIC_API_KEY set.');
    return;
  }
  // The MODEL attribute supplies the window (1000 tokens); NO compaction override.
  const { server, baseUrl } = spawnServer('compaction', PORT, {
    AWAKEN_MODEL_SOURCE: 'http',
    AWAKEN_MODEL_CONTEXT_WINDOW: '1000', // the model's published context window
    AWAKEN_COMPACT_TRIGGER_RATIO: '0.5',
    AWAKEN_COMPACT_KEEP_LAST: '2',
    // AWAKEN_COMPACT_MAX_TOKENS intentionally UNSET — the agent pins no window.
  });
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const s = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });

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
    assert.ok(
      compacted,
      `token-aware compaction fired with the window sourced from the MODEL attribute within ${turns} turns`,
    );
    pass(`compaction window inherited from the model attribute fired after ${turns} turns`);
    console.log('E2E PASS: compaction window derived from the model context_window attribute (no override).');
  } finally {
    await stopServer(server);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
