// Out-of-band memory e2e: a turn in session A triggers the background
// extractor sub-run (which saves a memory via the `write_memory` tool); a LATER
// session B sees that memory injected request-only by the recall plugin. The
// `memory` mode's probe model surfaces the injected context in its reply, so
// the whole loop — extract → store → recall → inject — is observable on the wire.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function reply(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events
    .filter((e) => e.type === 'agent.message')
    .map((e) => e.content.map((b) => b.text ?? '').join(''))
    .join('\n');
}

async function turn(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  return reply(client, sessionId);
}

async function main() {
  await withServer('memory', 38197, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    // Session A: the first turn has nothing to recall; its natural end fires
    // the background extractor, which saves the fixed memory.
    const a = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
    const first = await turn(client, a.id, 'remember the sky');
    assert.ok(first.includes('echo:remember the sky'), `probe echoes the turn: ${first}`);
    pass('session A turn completed (extractor fired in the background)');

    // Session B (later): the recall plugin injects the stored memory into the
    // request; the probe model surfaces it. The extractor is fire-and-forget,
    // so poll briefly.
    let recalled = '';
    for (let i = 0; i < 20; i += 1) {
      await new Promise((r) => setTimeout(r, 500));
      const b = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
      recalled = await turn(client, b.id, 'what color is the sky?');
      if (recalled.includes('sky is green')) break;
    }
    assert.ok(
      recalled.includes('sky is green'),
      `a later session sees the extracted memory in its injected context: ${recalled}`,
    );
    pass('extract -> store -> recall -> inject observable across sessions');

    // Accumulate enough distinct memories to cross the recall SELECTOR
    // threshold (default 12): each session saves a `fact-<n>` memory, then a
    // later recall runs the relevance selector over the grown store.
    for (let n = 0; n < 14; n += 1) {
      const s = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
      await turn(client, s.id, `remember fact-${n}`);
    }
    let selected = '';
    for (let i = 0; i < 20; i += 1) {
      await new Promise((r) => setTimeout(r, 500));
      const b = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
      selected = await turn(client, b.id, 'what facts do you recall about fact-13?');
      if (selected.includes('fact-')) break;
    }
    assert.ok(
      selected.includes('fact-'),
      `the relevance selector injected a memory from the grown store: ${selected}`,
    );
    pass('recall selector fires once the store passes its threshold (>12 memories)');
  });
  console.log('E2E PASS: out-of-band memory extraction + bounded recall across sessions.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
