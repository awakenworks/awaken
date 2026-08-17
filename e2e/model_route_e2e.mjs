// Model-routing Managed Agents e2e (R1/R2/R5/R6): a session binds its own model,
// the create response echoes it, and unknown per-event fields fail strictly
// without changing the Session route. The `model-route` server maps model refs
// to labeled executors
// (`fast`/`slow`/default), so the reply text `model=<label>` reveals which model
// (executor) each turn resolved to.
//
// Run: (from e2e/)  node model_route_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const agentWithModel = (model) => ({
  id: 'assistant',
  type: 'agent_with_overrides',
  model,
});

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

      // Cause-effect graph / decision table for the official model axis:
      // R1 override object model=fast -> echo fast and route the turn to fast;
      // R2 override object model=slow -> echo/route slow independently;
      // R3 model omitted -> inherit the host default;
      // R4 user.message model=slow is not part of the Managed event schema ->
      // reject before commit and preserve the frozen Session route.
      // Metadata is intentionally absent: it is descriptive data, never an
      // execution authority.
      const fast = await client.beta.sessions.create({
        agent: agentWithModel('fast'),
        environment_id: 'env_local',
        betas: BETAS,
      });
      assert.equal(fast.agent.model.id, 'fast', 'R6: create echoes the requested model (ModelConfig)');
      await ask(client, fast.id, 'hi');
      let texts = await latestAgentText(client, fast.id);
      assert.ok(texts.some((t) => t.startsWith('model=fast')), `R2: fast session ran fast, got ${texts}`);
      pass('per-session model "fast" resolves + echoes (R1/R2/R6)');

      // The provider/dialect/endpoint chain is parsed by the same open model-id
      // codec as production. Provider names are not allowlisted: a connected
      // third-party identity can select the ordinary Native runtime.
      const thirdPartyModel =
        'fast;provider=third-party%2Fgateway;api=open_ai_chat;endpoint=primary';
      const thirdParty = await client.beta.sessions.create({
        agent: agentWithModel(thirdPartyModel),
        environment_id: 'env_local',
        betas: BETAS,
      });
      assert.equal(thirdParty.agent.model.id, thirdPartyModel);
      await ask(client, thirdParty.id, 'third party');
      texts = await latestAgentText(client, thirdParty.id);
      assert.ok(
        texts.some((t) => t.startsWith('model=fast')),
        'third-party route: ' + JSON.stringify(texts),
      );
      pass('opaque third-party provider chain resolves through the Native runtime');

      const sessionIdsBeforeInvalid = [];
      for await (const listed of client.beta.sessions.list({ betas: BETAS })) {
        sessionIdsBeforeInvalid.push(listed.id);
      }
      await assert.rejects(
        () => client.beta.sessions.create({
          agent: agentWithModel('fast;api=open_ai_chat'),
          environment_id: 'env_local',
          betas: BETAS,
        }),
        (error) => error.status === 400,
      );
      const sessionIdsAfterInvalid = [];
      for await (const listed of client.beta.sessions.list({ betas: BETAS })) {
        sessionIdsAfterInvalid.push(listed.id);
      }
      assert.deepEqual(sessionIdsAfterInvalid.sort(), sessionIdsBeforeInvalid.sort());
      pass('dialect without provider fails before Session persistence');

      // A second session bound to `slow` resolves a distinct executor.
      const slow = await client.beta.sessions.create({
        agent: agentWithModel('slow'),
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

      // R5: model selection is Session-scoped in the Managed wire contract.
      // A model-shaped unknown event field must be rejected strictly and may
      // neither commit a turn nor mutate the already-frozen route.
      const sw = await client.beta.sessions.create({
        agent: agentWithModel('fast'),
        environment_id: 'env_local',
        betas: BETAS,
      });
      await ask(client, sw.id, 'first');
      await assert.rejects(() => ask(client, sw.id, 'rejected', 'slow'), (error) => error.status === 400);
      await ask(client, sw.id, 'second');
      texts = await latestAgentText(client, sw.id);
      assert.equal(texts.filter((t) => t.startsWith('model=fast')).length, 2, `R5: route changed, got ${texts}`);
      assert.ok(!texts.some((t) => t.startsWith('model=slow')), `R5: rejected model leaked, got ${texts}`);
      pass('unknown per-event model is rejected without route mutation (R5)');
    });

    console.log('E2E PASS: Session-scoped model routing + strict event schema (R1/R2/R5/R6).');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
