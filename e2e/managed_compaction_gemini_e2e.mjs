// Real-LLM token-aware compaction e2e (Gemini). Drives a managed session through
// the official @anthropic-ai/sdk against awaken-server in `compaction` mode
// backed by a REAL model (Gemini AI Studio via AWAKEN_MODEL_SOURCE=gemini), with a
// small *configured* context window (AWAKEN_COMPACT_MAX_TOKENS) so a handful of
// large turns crosses the trigger ratio and the compactor sub-agent — itself a
// real model run — folds the older slice. Proves the token-aware trigger fires
// against a live LLM with large real input/output and surfaces
// `agent.thread_context_compacted` on the wire, then keeps replying afterward.
//
// Every model call crosses the real genai provider adapter + a real socket + the
// real Gemini wire; only the remote endpoint differs from the KIMI-specific
// managed_compaction_real_e2e.mjs (KIMI creds are dead; a Google key is live).
//
// Run: GEMINI_API_KEY=... (or GOOGLE_API_KEY=...) node managed_compaction_gemini_e2e.mjs
//   Self-skips when no Google/Gemini key is set.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38261);
const BETAS = ['managed-agents-2026-04-01'];
const KEY = process.env.GEMINI_API_KEY || process.env.GOOGLE_API_KEY;

async function allEvents(client, id) {
  const evs = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) evs.push(ev);
  return evs;
}

async function main() {
  if (!KEY) {
    console.log('SKIP managed_compaction_gemini_e2e: no GEMINI_API_KEY / GOOGLE_API_KEY set.');
    return;
  }
  // A small configured window: fold once the transcript reaches 0.5 * 1200 = 600
  // estimated tokens, keeping the last 2 messages verbatim.
  const { server, baseUrl } = spawnServer('compaction', PORT, {
    AWAKEN_MODEL_SOURCE: 'gemini',
    GEMINI_API_KEY: KEY,
    GOOGLE_API_KEY: KEY,
    GEMINI_MODEL: process.env.GEMINI_MODEL ?? 'gemini-2.5-flash',
    AWAKEN_COMPACT_MAX_TOKENS: '1200',
    AWAKEN_COMPACT_TRIGGER_RATIO: '0.5',
    AWAKEN_COMPACT_KEEP_LAST: '2',
  });
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const s = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });

    // Each turn carries a large message; after a few, the committed transcript
    // exceeds the ~600-token budget and the older slice folds via a real
    // compactor sub-run.
    const big = (n) =>
      `Message ${n}. Please acknowledge briefly. ` +
      'Keep the following context in mind for later questions. '.repeat(60);
    let compacted = false;
    let turns = 0;
    for (let i = 1; i <= 8 && !compacted; i += 1) {
      turns = i;
      await client.beta.sessions.events.send(s.id, {
        betas: BETAS,
        events: [{ type: 'user.message', content: [{ type: 'text', text: big(i) }] }],
      });
      const types = (await allEvents(client, s.id)).map((e) => e.type);
      assert.ok(types.includes('agent.message'), `turn ${i}: the real model replied — ${types.join(',')}`);
      compacted = types.includes('agent.thread_context_compacted');
    }
    assert.ok(compacted, `token-aware compaction fired against real Gemini within ${turns} turns`);
    pass(`real-Gemini token-aware compaction fired after ${turns} turns of large I/O`);

    // The projected event carries the SDK shape.
    const ev = (await allEvents(client, s.id)).find((e) => e.type === 'agent.thread_context_compacted');
    assert.equal(typeof ev.id, 'string');
    assert.equal(typeof ev.processed_at, 'string');
    pass('agent.thread_context_compacted has the SDK shape (id + processed_at)');

    // The session keeps working after the fold — a further turn still replies.
    await client.beta.sessions.events.send(s.id, {
      betas: BETAS,
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'Thanks — one word: ok?' }] }],
    });
    const after = (await allEvents(client, s.id)).filter((e) => e.type === 'agent.message');
    assert.ok(after.length >= turns + 1, `session still replies after the fold (${after.length} agent.message)`);
    pass('the session keeps replying after compaction');

    console.log('E2E PASS: real-Gemini token-aware compaction (fold at a fraction of max_tokens, live LLM).');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server);
  }
}

main();
