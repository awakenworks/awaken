// Real-model `agent.thinking` (events/reference "Agent events"): a thinking-capable
// provider's extended-thinking must surface as the contentless `agent.thinking`
// marker (`BetaManagedAgentsAgentThinkingEvent` = `{id, processed_at, type}`, "a
// progress signal, not a content carrier"), emitted separately from `agent.message`.
//
// This validates the reasoning path end to end: genai captures the provider's
// reasoning -> a folded `Thinking` block -> `Fact::AssistantThinking` -> the Managed
// wire's `agent.thinking`. KIMI (kimi-k2) emits thinking blocks, so a step-by-step
// prompt reliably produces one. The reasoning text itself is intentionally NOT on the
// wire (the marker carries none); the answer still lands in `agent.message`.
//
// Gated: skips without a real key. Run: (from e2e/, with KIMI env)
//   ANTHROPIC_API_KEY=sk-kimi-... ANTHROPIC_BASE_URL=https://api.kimi.com/coding/v1/ \
//   ANTHROPIC_MODEL=kimi-for-coding node managed_real_thinking_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38254);
const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  if (!process.env.ANTHROPIC_API_KEY && !process.env.KIMI_API_KEY) {
    console.log('SKIP managed_real_thinking_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  try {
    await withServer('real', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'Think step by step, then answer: a bat and a ball cost $1.10 total, and the bat costs $1.00 more than the ball. How many cents is the ball? Reply with just the number.' }] }],
        betas: BETAS,
      });

      const events = [];
      for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);

      const thinking = events.filter((e) => e.type === 'agent.thinking');
      assert.ok(thinking.length >= 1, `expected an agent.thinking event, saw: ${[...new Set(events.map((e) => e.type))].join(', ')}`);
      pass(`surfaced ${thinking.length} agent.thinking marker(s)`);

      // The marker is contentless (a progress signal): id + processed_at + type only.
      const mark = thinking[0];
      assert.equal(mark.type, 'agent.thinking');
      assert.ok(mark.id, 'agent.thinking carries an id');
      assert.ok(mark.processed_at, 'agent.thinking carries processed_at');
      // No reasoning content leaks onto the wire (the SDK type has no content field).
      assert.equal(mark.content, undefined, 'agent.thinking carries no content field');
      assert.equal(mark.thinking, undefined, 'agent.thinking carries no thinking text');
      pass('agent.thinking is a contentless marker (id/processed_at/type only)');

      // Thinking precedes the answer, and the answer still lands in agent.message.
      const idxThink = events.findIndex((e) => e.type === 'agent.thinking');
      const idxMsg = events.findIndex((e) => e.type === 'agent.message');
      assert.ok(idxMsg === -1 || idxThink < idxMsg, 'agent.thinking is emitted before agent.message');
      const finalMsg = events.filter((e) => e.type === 'agent.message').at(-1);
      assert.ok(finalMsg, 'the answer still lands in agent.message');
      assert.match(JSON.stringify(finalMsg.content), /5/, `expected the correct answer (5 cents): ${JSON.stringify(finalMsg.content)}`);
      pass('answer lands in agent.message (5 cents); thinking precedes it');

      console.log('E2E PASS: real-model agent.thinking marker surfaced (contentless) alongside the answer.');
    });
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exit(1);
  }
}

main();
