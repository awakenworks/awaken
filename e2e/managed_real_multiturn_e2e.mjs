// Real-model multi-Run context (Sessions / quickstart "stateful sessions"): a
// second User Run must see the first Run's content from the server-persisted
// history — the "persistent conversation history across interactions" promise,
// validated with a live KIMI model rather than an echo stub that trivially replays.
//
// Cause/effect graph: C1=Run one commits two distinct facts; C2/C3=later Runs
// ask for each fact on the same Session. Effects: E1=each exact User Event
// receipt becomes processed before its later Agent Message/idle; E2/E3=the live
// model recalls durian/47 from committed history. Decision rules:
// M1(C1)->E1; M2(C1+C2)->E1+E2; M3(C1+C3)->E1+E3. Two facts prevent a lucky
// guess from satisfying the history contract. Constraints/invariant: every
// answer must follow its exact processed receipt on the same durable Session;
// a live-provider smoke cannot substitute for deterministic protocol gates.
//
// Gated: skips without a real key. Run: (from e2e/, with a live key in the
// controller environment or ~/.bashrc; it reaches the server only through the
// config-plane API)  node managed_real_multiturn_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { pass, waitForSessionEventReceipt, withServer, modelUpstreamEnv } from './harness.mjs';
import { liveAnthropicCompatibleUpstream } from './model_upstream.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38253);
const BETAS = ['managed-agents-2026-04-01'];

async function ask(client, id, text) {
  const receipt = await client.beta.sessions.events.send(id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const acceptedId = receipt.data[0]?.id;
  assert.equal(typeof acceptedId, 'string', 'M1-M3 exact accepted User Event id');
  const { delta: events } = await waitForSessionEventReceipt(
    client,
    id,
    acceptedId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    `live Run for ${JSON.stringify(text)} to settle`,
    { timeoutMs: 300_000, pollMs: 250 },
  );
  // The Agent Message produced by this Run excludes prior-Run history.
  const msgs = events.filter((event) => event.type === 'agent.message');
  return JSON.stringify(msgs.at(-1)?.content ?? '');
}

async function main() {
  const live = liveAnthropicCompatibleUpstream();
  if (!live) {
    console.log('SKIP managed_real_multiturn_e2e: no live Anthropic-compatible key (ANTHROPIC_API_KEY or ~/.bashrc).');
    return;
  }
  try {
    await withServer('real', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

      await ask(client, session.id, 'Remember these two facts for later: my favorite fruit is durian, and my lucky number is 47. Just acknowledge.');
      pass('Run 1: established two facts on the Session');

      const fruit = await ask(client, session.id, 'What is my favorite fruit? Reply with just the one word.');
      assert.match(fruit.toLowerCase(), /durian/, `Run 2 lost the fruit fact: ${fruit}`);
      pass('Run 2: real model recalled the fruit from Run 1 history');

      const number = await ask(client, session.id, 'What is my lucky number? Reply with just the number.');
      assert.match(number, /47/, `Run 3 lost the number fact: ${number}`);
      pass('Run 3: real model recalled the number from Run 1 history');

      console.log('E2E PASS: real-model multi-Run context carryover across a persistent Session.');
    }, modelUpstreamEnv(null, live));
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exit(1);
  }
}

main();
