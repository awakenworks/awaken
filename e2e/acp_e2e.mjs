// ACP-runtime Managed Agents e2e (R3/R4/R7): an immutable Agent publication
// selects `acp:claude` and runs on an external ACP CLI (a fake pinned adapter
// stand-in launched as a subprocess), while a native session on the same server
// runs the built-in echo model. A second turn re-launches the ACP CLI (R7).
//
// Run: (from e2e/)  node acp_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  withScenarioServer,
  pass,
  waitForValue,
  waitForSessionEventReceipt,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const XLSX_SKILL = [{ type: 'anthropic', skill_id: 'xlsx', version: '1' }];

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(ev);
  }
  return events;
}

function agentTexts(events) {
  return events
    .filter((e) => e.type === 'agent.message')
    .map((m) => (m.content ?? []).map((c) => c.text ?? '').join('').trim());
}

async function waitForAgentText(client, sessionId, predicate) {
  // Create-time initial_events expose no standalone receipt, so the canonical
  // bounded observer waits on the Session-owned history itself. C1=create
  // accepted initial input; C2=matching Agent text. E1=C2; K=no runtime drive.
  // Decision I1 C1&&!C2=>retry; I2 C1+C2=>return the last history.
  const events = await waitForValue(
    () => listEvents(client, sessionId),
    (listed) => agentTexts(listed).some(predicate),
    'ACP create-time initial Event to commit its matching Agent text',
  );
  return agentTexts(events);
}

async function send(client, sessionId, text) {
  // C1=exact mid-Session User receipt; C2=ACP/native reply+terminal. E1=C2
  // after C1. K: each fresh ACP process is proved by its own receipt. Decision
  // A1 C1&&!C2=>retry; A2 C1+C2=>return committed history.
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'A1 exact ACP/native User Event receipt');
  return waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    `A1 ACP/native Run for ${JSON.stringify(text)} to commit`,
  );
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
      // | A3 | acp:claude | later User + final System batch | fresh ACP process + exact System receipt |
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
      pass('the published assistant runs on the external ACP CLI via the managed API (R3/R4)');

      // Selection: a native session on the same server runs the built-in model.
      const native = await client.beta.sessions.create({
        agent: {
          id: 'native-assistant',
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

      // A3/R7 causes: C1=a continued User event; C2=one System event is final and
      // immediately follows C1 in the same batch; C3=the selected ACP model
      // supports dynamic System context. Effects: E1=receipts preserve User then
      // System order; E2=the exact final System receipt commits with the same
      // history id; E3=one fresh ACP process replies and the Session idles.
      // Constraint: the atomic batch is the sole admission boundary and the
      // Session root remains the sole System owner. Decision A3 C1+C2+C3=>
      // E1+E2+E3; a standalone System is owned by the admission rejection table.
      const systemReceipt = await client.beta.sessions.events.send(acp.id, {
        events: [
          { type: 'user.message', content: [{ type: 'text', text: 'again' }] },
          { type: 'system.message', content: [{ type: 'text', text: 'be concise' }] },
        ],
        betas: BETAS,
      });
      assert.deepEqual(
        systemReceipt.data.map((event) => event.type),
        ['user.message', 'system.message'],
        'A3/E1 ACP continuation receipts preserve the admitted batch order',
      );
      const systemReceiptId = systemReceipt.data[1]?.id;
      assert.equal(typeof systemReceiptId, 'string', 'A3/E2 exact final ACP System receipt');
      const secondAcp = await waitForSessionEventReceipt(
        client,
        acp.id,
        systemReceiptId,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message')
          && delta.some((event) => event.type === 'session.status_idle'),
        'A3/E3 ACP continuation after the exact final System receipt to commit',
      );
      texts = agentTexts(secondAcp.events);
      const acpReplies = texts.filter((t) => t.includes('acp-runtime reply')).length;
      const newAcpReplies = agentTexts(secondAcp.delta)
        .filter((text) => text.includes('acp-runtime reply')).length;
      assert.equal(newAcpReplies, 1, 'A3/E3 the atomic continuation drives exactly one fresh ACP reply');
      assert.equal(acpReplies, 2, `R7: each turn relaunches the CLI exactly once, got ${acpReplies} replies`);
      const acpEvents = secondAcp.events;
      assert.ok(
        acpEvents.some((event) => event.type === 'system.message' && event.id === systemReceiptId),
        'A3/E2 ACP history persists the exact same-id System event',
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
        const texts = agentTexts((await send(client, s2.id, trigger)).events);
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
