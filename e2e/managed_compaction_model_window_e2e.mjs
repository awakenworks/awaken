// E2E: token-aware compaction whose window comes from the MODEL ATTRIBUTE, not a
// compaction override. `AWAKEN_COMPACT_MAX_TOKENS` is deliberately UNSET; only the
// model's published context window (`AWAKEN_MODEL_CONTEXT_WINDOW` — this harness's
// projection of the catalog's `ModelSpec.context_window`) is configured. So the fold
// firing proves Config's typed default derived the effective window from the
// model attribute end to end, against a live KIMI model.
//
// Run: KIMI_API_KEY=... node managed_compaction_model_window_e2e.mjs (self-skips w/o key).

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  pass,
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38261);
const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  if (!process.env.KIMI_API_KEY && !process.env.ANTHROPIC_API_KEY) {
    console.log('SKIP managed_compaction_model_window_e2e: no KIMI_API_KEY / ANTHROPIC_API_KEY set.');
    return;
  }
  // The MODEL attribute supplies the window (1000 tokens); NO compaction override.
  const { server, baseUrl } = spawnServer('compaction', PORT, {
    AWAKEN_MODEL_SOURCE: 'http',
    AWAKEN_MODEL_CONTEXT_WINDOW: '1000', // the model's published context window
    AWAKEN_COMPACT_KEEP_LAST: '2',
    // AWAKEN_COMPACT_MAX_TOKENS intentionally UNSET — the agent pins no window.
  });
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

    const big = (n) => `Message ${n}. ` + 'Please keep this context in mind. '.repeat(40);
    let compacted = false;
    let turns = 0;
    // Model-window decision M1: C1 exact per-turn receipt and C2 live reply;
    // C3 cumulative model-owned window threshold. E1 receipt-scoped reply;
    // E2 compaction after C3. K1 earlier replies cannot satisfy a later turn.
    // D1=C1+C2=>E1; D2=D1+C3=>E2.
    for (let i = 1; i <= 6 && !compacted; i += 1) {
      turns = i;
      const receipt = (await client.beta.sessions.events.send(s.id, {
        betas: BETAS,
        events: [{ type: 'user.message', content: [{ type: 'text', text: big(i) }] }],
      })).data[0];
      const observation = await waitForSessionEventReceipt(
        client,
        s.id,
        receipt.id,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message'),
        `model-window compaction turn ${i}`,
        { timeoutMs: 180_000 },
      );
      const types = observation.events.map((e) => e.type);
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
