// ACP-runtime Managed Agents e2e (R3/R4/R7): an immutable Agent publication
// selects `acp:claude` and runs on an external ACP CLI (a fake `claude --acp`
// stand-in launched as a subprocess), while a native session on the same server
// runs the built-in echo model. A second turn re-launches the ACP CLI (R7).
//
// Run: (from e2e/)  node acp_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const XLSX_SKILL = [{ type: 'anthropic', skill_id: 'xlsx', version: '1' }];

async function agentTexts(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(ev);
  }
  return events
    .filter((e) => e.type === 'agent.message')
    .map((m) => (m.content ?? []).map((c) => c.text ?? '').join('').trim());
}

async function waitForAgentText(client, sessionId, predicate) {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    const texts = await agentTexts(client, sessionId);
    if (texts.some(predicate)) return texts;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  return agentTexts(client, sessionId);
}

async function send(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

async function main() {
  try {
    // Port distinct from managed_durable_e2e (38170): that test restarts its
    // server and its process-level dispatch pool lingers briefly on the port, so
    // sharing 38170 cross-contaminates in a full-suite run. 38185 is unshared.
    await withScenarioServer('acp', 'echo', 38185, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // Cause graph / decision rules shared with the native case below:
      // selected backend (ACP/native) + valid create-time user.message -> create
      // returns running -> the common Session event executor persists the input ->
      // the selected runtime emits its own reply -> Session idles.
      //
      // | Rule | Backend | Skill source | Trigger | Expected reply/pin |
      // | A1 | acp:claude | Anthropic xlsx@1 | initial_events | ACP fixture + exact selection |
      // | A2 | native | Anthropic xlsx@1 | initial_events | built-in echo + exact selection |
      // | A3 | acp:claude | system.message + later events.send | fresh ACP process |
      //
      // R3/R4/A1: the published ACP Agent runs on the external CLI. Request
      // metadata is not a backend selector.
      const acp = await client.beta.sessions.create({
        agent: {
          id: 'acp-agent',
          type: 'agent_with_overrides',
          skills: XLSX_SKILL,
        },
        environment_id: 'env_local',
        initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'hello' }] }],
        betas: BETAS,
      });
      assert.equal(acp.status, 'running', 'A1 create-time ACP event starts immediately');
      assert.deepEqual(acp.agent.skills, XLSX_SKILL, 'A1 ACP Session accepts the prebuilt pin');
      let texts = await waitForAgentText(client, acp.id, (text) => text.includes('acp-runtime reply'));
      assert.ok(
        texts.some((t) => t.includes('acp-runtime reply')),
        `R3/R4: acp session ran on the ACP CLI, got ${JSON.stringify(texts)}`,
      );
      pass('runtime:"acp:claude" runs on the external ACP CLI via the managed API (R3/R4)');

      // Selection: a native session on the same server runs the built-in model.
      const native = await client.beta.sessions.create({
        agent: {
          id: 'assistant',
          type: 'agent_with_overrides',
          skills: XLSX_SKILL,
        },
        environment_id: 'env_local',
        initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'hello' }] }],
        betas: BETAS,
      });
      assert.equal(native.status, 'running', 'A2 create-time native event starts immediately');
      assert.deepEqual(native.agent.skills, XLSX_SKILL, 'A2 native Session accepts the same pin');
      texts = await waitForAgentText(client, native.id, (text) => text.startsWith('Echo:'));
      assert.ok(
        texts.some((t) => t.startsWith('Echo:')),
        `native session ran the built-in model, got ${JSON.stringify(texts)}`,
      );
      assert.ok(
        !texts.some((t) => t.includes('acp-runtime')),
        'native session must NOT hit the ACP CLI',
      );
      pass('a native session on the same server runs the built-in runtime (selection)');

      // A3/R7: the ACP adapter accepts the same mid-conversation system event,
      // persists it, and a second user turn re-launches the CLI (fresh per turn).
      const systemReceipt = await client.beta.sessions.events.send(acp.id, {
        events: [{ type: 'system.message', content: [{ type: 'text', text: 'be concise' }] }],
        betas: BETAS,
      });
      assert.equal(systemReceipt.data[0].type, 'system.message', 'A3 ACP system event admitted');
      await send(client, acp.id, 'again');
      texts = await agentTexts(client, acp.id);
      const acpReplies = texts.filter((t) => t.includes('acp-runtime reply')).length;
      assert.ok(acpReplies >= 2, `R7: each turn relaunches the CLI, got ${acpReplies} acp replies`);
      const acpEvents = [];
      for await (const event of client.beta.sessions.events.list(acp.id, { betas: BETAS })) {
        acpEvents.push(event);
      }
      assert.ok(
        acpEvents.some((event) => event.type === 'system.message' && event.id === systemReceipt.data[0].id),
        'A3 ACP history persists the same-id system event',
      );
      pass('a second turn relaunches the ACP CLI (R7)');

      // Driver-error paths reachable through the fake CLI: a malformed frame
      // and a truncated stream both surface a classified failure message. (A
      // refusal turn_end renders as a normal idle with no distinct wire signal,
      // and the provider-keyed taxonomy — auth/rate-limit/login — only arises
      // from a real CLI's output, so those stay unit-tested.)
      for (const trigger of ['acp-auth reply', 'acp-truncate reply']) {
        const s2 = await client.beta.sessions.create({
          agent: 'acp-agent',
          environment_id: 'env_local',
          betas: BETAS,
        });
        await send(client, s2.id, trigger);
        const texts = await agentTexts(client, s2.id);
        assert.ok(
          texts.some((t) => /the agent turn failed/i.test(t)),
          `failure "${trigger}" rendered a classified failure, got ${JSON.stringify(texts)}`,
        );
      }
      pass('ACP driver failures classify + render (malformed frame + truncation)');
    });

    console.log('E2E PASS: ACP-runtime selection + relaunch (R3/R4/R7) via the managed API.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
