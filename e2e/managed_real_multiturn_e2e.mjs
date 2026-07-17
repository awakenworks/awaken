// Real-model multi-turn context (sessions / quickstart "stateful sessions"): a
// second user turn must see the first turn's content from the server-persisted
// history — the "persistent conversation history across interactions" promise,
// validated with a live KIMI model rather than an echo stub that trivially replays.
//
// Design: state carryover across turns — establish a fact in turn 1, query it in
// turn 2 on the SAME session, assert the real model's answer reflects turn 1. Two
// distinct facts (fruit, number) so a lucky guess can't pass.
//
// Gated: skips without a real key. Run: (from e2e/, with KIMI env)
//   ANTHROPIC_API_KEY=sk-kimi-... ANTHROPIC_BASE_URL=https://api.kimi.com/coding/v1/ \
//   ANTHROPIC_MODEL=kimi-k2-0711-preview node managed_real_multiturn_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38253);
const BETAS = ['managed-agents-2026-04-01'];

async function ask(client, id, text) {
  const before = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) before.push(ev.id);
  const seen = new Set(before);
  await client.beta.sessions.events.send(id, { events: [{ type: 'user.message', content: [{ type: 'text', text }] }], betas: BETAS });
  // The last agent.message produced by this turn (skip prior-turn history).
  const msgs = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) {
    if (!seen.has(ev.id) && ev.type === 'agent.message') msgs.push(ev);
  }
  return JSON.stringify(msgs.at(-1)?.content ?? '');
}

async function main() {
  if (!process.env.ANTHROPIC_API_KEY && !process.env.KIMI_API_KEY) {
    console.log('SKIP managed_real_multiturn_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  try {
    await withServer('real', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

      await ask(client, session.id, 'Remember these two facts for later: my favorite fruit is durian, and my lucky number is 47. Just acknowledge.');
      pass('turn 1: established two facts on the session');

      const fruit = await ask(client, session.id, 'What is my favorite fruit? Reply with just the one word.');
      assert.match(fruit.toLowerCase(), /durian/, `turn 2 lost the fruit fact: ${fruit}`);
      pass('turn 2: real model recalled the fruit from turn 1 history');

      const number = await ask(client, session.id, 'What is my lucky number? Reply with just the number.');
      assert.match(number, /47/, `turn 3 lost the number fact: ${number}`);
      pass('turn 3: real model recalled the number from turn 1 history');

      console.log('E2E PASS: real-model multi-turn context carryover across a persistent session.');
    });
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exit(1);
  }
}

main();
