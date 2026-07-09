// Model-routing Managed Agents e2e (R1/R2/R5/R6): a session binds its own model,
// the create response echoes it, and a per-turn `model` on user.message switches
// mid-conversation. The `model-route` server maps model refs to labeled executors
// (`fast`/`slow`/default), so the reply text `model=<label>` reveals which model
// (executor) each turn resolved to.
//
// Run: (from e2e/)  node model_route_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function latestAgentText(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(ev);
  }
  const msgs = events.filter((e) => e.type === 'agent.message');
  const texts = msgs.map((m) => (m.content ?? []).map((c) => c.text ?? '').join('').trim());
  return texts;
}

async function ask(client, sessionId, text, model) {
  const ev = { type: 'user.message', content: [{ type: 'text', text }] };
  if (model) ev.model = model;
  await client.beta.sessions.events.send(sessionId, { events: [ev], betas: BETAS });
}

async function main() {
  try {
    await withScenarioServer('model-route', 'label', 38160, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // R1/R2/R6: a session bound to `fast` resolves the fast executor and echoes it.
      const fast = await client.beta.sessions.create({
        agent: 'assistant', metadata: { 'awaken.model': 'fast' },
        environment_id: 'env_local',
        betas: BETAS,
      });
      assert.equal(fast.agent.model.id, 'fast', 'R6: create echoes the requested model (ModelConfig)');
      await ask(client, fast.id, 'hi');
      let texts = await latestAgentText(client, fast.id);
      assert.ok(texts.some((t) => t.startsWith('model=fast')), `R2: fast session ran fast, got ${texts}`);
      pass('per-session model "fast" resolves + echoes (R1/R2/R6)');

      // A second session bound to `slow` resolves a distinct executor.
      const slow = await client.beta.sessions.create({
        agent: 'assistant', metadata: { 'awaken.model': 'slow' },
        environment_id: 'env_local',
        betas: BETAS,
      });
      await ask(client, slow.id, 'hi');
      texts = await latestAgentText(client, slow.id);
      assert.ok(texts.some((t) => t.startsWith('model=slow')), `R2: slow session ran slow, got ${texts}`);
      pass('a different session binds a different model (R1/R2)');

      // A session with no model uses the host default.
      const def = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      await ask(client, def.id, 'hi');
      texts = await latestAgentText(client, def.id);
      assert.ok(texts.some((t) => t.startsWith('model=default')), `default session ran default, got ${texts}`);
      pass('no model → host default (backward compatible)');

      // R5: a per-turn `model` override switches the thread mid-conversation.
      const sw = await client.beta.sessions.create({
        agent: 'assistant', metadata: { 'awaken.model': 'fast' },
        environment_id: 'env_local',
        betas: BETAS,
      });
      await ask(client, sw.id, 'first');            // runs fast
      await ask(client, sw.id, 'second', 'slow');   // per-turn override → slow
      texts = await latestAgentText(client, sw.id);
      assert.ok(texts.some((t) => t.startsWith('model=fast')), `R5: first turn fast, got ${texts}`);
      assert.ok(texts.some((t) => t.startsWith('model=slow')), `R5: overridden turn slow, got ${texts}`);
      pass('per-turn model override switches mid-conversation (R5)');
    });

    console.log('E2E PASS: per-session + per-turn model routing (R1/R2/R5/R6) via the managed API.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
