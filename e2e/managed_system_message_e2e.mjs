// Mid-Session system.message through the official Anthropic TypeScript SDK.
//
// Cause/effect graph: C1=one System event; C2=final; C3=immediately after a
// User message or exact custom result; C4=text-only; C5=model supports dynamic
// System context. Effects:
// E1=the whole batch is accepted in public order; E2=the System MessageId commits
// in the accompanying Run; E3=the context remains installed for later Runs;
// E4=invalid combinations return 400 with no Event or Run. Decision rules:
// S1(User+C1-C5)->E1+E2+E3; S2(custom-result+C1-C5)->E1+E2+E3;
// S3(standalone/nonfinal/multiple)->E4. The existing MCP Sessions-family owner
// covers Confirmation+System rejection. The generic-result rule remains blocked
// on a reachable self-hosted AgentToolset pending state and is not fabricated.
// Constraints/invariant: System is final, text-only, and attached to the same
// atomic Event batch/Run; it never creates a separate context authority.
//
// Run: (from e2e/)  node managed_system_message_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  pass,
  waitForSessionEventReceipt,
  withRealServer,
  withScenarioServer,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38404);

async function sendUser(client, sid, text) {
  return client.beta.sessions.events.send(sid, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}
async function listTypes(client, sid) {
  const evs = [];
  for await (const e of client.beta.sessions.events.list(sid, { betas: BETAS })) evs.push(e);
  return evs;
}

async function expectInvalid(client, sessionID, events, rule) {
  await assert.rejects(
    client.beta.sessions.events.send(sessionID, { events, betas: BETAS }),
    (error) => error?.status === 400,
    rule,
  );
}

async function main() {
  try {
    await withRealServer('echo', PORT, async (baseUrl, upstream) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

      const system = { type: 'system.message', content: [{ type: 'text', text: 'Be terse from now on.' }] };
      const user = { type: 'user.message', content: [{ type: 'text', text: 'first' }] };
      await expectInvalid(client, session.id, [system], 'S2 rejects standalone System');
      await expectInvalid(
        client,
        session.id,
        [user, system, { type: 'user.interrupt' }],
        'S2 rejects non-final System',
      );
      await expectInvalid(client, session.id, [user, system, system], 'S2 rejects multiple System events');
      assert.deepEqual(await listTypes(client, session.id), [], 'S2 has no partial Event or Run');

      // The valid User+System pair is acknowledged in wire order. Depending on
      // scheduling, the response can race the same Run's commit, so the System
      // receipt may already be processed; the durable history must converge on
      // the same id with a timestamp in either case.
      const receipt = await client.beta.sessions.events.send(session.id, {
        events: [user, system],
        betas: BETAS,
      });
      assert.deepEqual(receipt.data.map((event) => event.type), ['user.message', 'system.message']);
      assert.ok(
        receipt.data[1].processed_at === null || typeof receipt.data[1].processed_at === 'string',
        'System receipt reflects whichever side of the concurrent Thread commit won',
      );
      const systemReceiptId = receipt.data[1]?.id;
      assert.equal(typeof systemReceiptId, 'string', 'S1 exact final System Event receipt');
      let { events } = await waitForSessionEventReceipt(
        client,
        session.id,
        systemReceiptId,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message')
          && delta.some((event) => event.type === 'session.status_idle'),
        'the accompanying Run to commit User and System input',
      );
      const persistedSystem = events.find((event) => event.type === 'system.message');
      assert.equal(persistedSystem?.id, receipt.data[1].id, 'receipt and history share System id');
      assert.ok(persistedSystem?.processed_at, 'the accompanying Run committed the System MessageId');
      const status = (await client.beta.sessions.retrieve(session.id, { betas: BETAS })).status;
      assert.equal(status, 'idle', 'the Session is idle after the accompanying Run');
      pass('valid User+System batch commits both inputs through one Run');

      // The Session keeps working and retains the Session-root System context.
      // Provider input, not the echo text, is the authoritative evidence.
      const requestsBeforeSecondRun = upstream.requests.length;
      const secondReceipt = await sendUser(client, session.id, 'second');
      const secondReceiptId = secondReceipt.data[0]?.id;
      assert.equal(typeof secondReceiptId, 'string', 'S1 later Run exact User Event receipt');
      ({ events } = await waitForSessionEventReceipt(
        client,
        session.id,
        secondReceiptId,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message')
          && delta.some((event) => event.type === 'session.status_idle'),
        'the later Run to finish',
      ));
      const echoes = events.filter((e) => e.type === 'agent.message').map((e) => e.content?.[0]?.text);
      assert.ok(echoes.includes('Echo: second'), `later Run still executes (echoes: ${JSON.stringify(echoes)})`);
      const laterRequests = upstream.requests.slice(requestsBeforeSecondRun);
      assert.equal(laterRequests.length, 1, 'the later User input produces one Provider request');
      assert.match(
        laterRequests[0].system,
        /Be terse from now on\./u,
        'the later Provider request contains the durable Session System context',
      );
      pass('later Run executes with the same durable Session context');
    });

    await withScenarioServer('custom', 'custom', PORT + 1, async (baseUrl, upstream) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      const initialReceipt = await sendUser(client, session.id, 'request the custom answer');
      const initialReceiptId = initialReceipt.data[0]?.id;
      assert.equal(typeof initialReceiptId, 'string', 'S2 exact initial User Event receipt');
      const { events: pending } = await waitForSessionEventReceipt(
        client,
        session.id,
        initialReceiptId,
        BETAS,
        ({ delta }) => {
          const customUse = delta.find((event) => event.type === 'agent.custom_tool_use');
          const idle = [...delta].reverse().find((event) => event.type === 'session.status_idle');
          return customUse
            && idle?.stop_reason?.type === 'requires_action'
            && idle.stop_reason.event_ids.includes(customUse.id);
        },
        'S2 exact custom tool use to become durably pending',
      );
      const customUse = pending.find((event) => event.type === 'agent.custom_tool_use');
      const customSystem = {
        type: 'system.message',
        content: [{ type: 'text', text: 'SYSTEM-CUSTOM-CONTEXT' }],
      };
      const receipt = await client.beta.sessions.events.send(session.id, {
        events: [{
          type: 'user.custom_tool_result',
          custom_tool_use_id: customUse.id,
          content: [{ type: 'text', text: '42' }],
        }, customSystem],
        betas: BETAS,
      });
      assert.deepEqual(
        receipt.data.map((event) => event.type),
        ['user.custom_tool_result', 'system.message'],
        'S2 acknowledges the typed result and adjacent System in public order',
      );
      const finalReceiptId = receipt.data.at(-1)?.id;
      assert.equal(typeof finalReceiptId, 'string', 'S2 exact final System Event receipt');
      const { events: completed } = await waitForSessionEventReceipt(
        client,
        session.id,
        finalReceiptId,
        BETAS,
        ({ events, delta }) => receipt.data.every((item) =>
          events.some((event) => event.id === item.id && event.processed_at))
          && delta.some((event) => event.type === 'agent.message' && event.content?.[0]?.text?.includes('42'))
          && [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn',
        'S2 custom result and System to commit through one continuation',
      );
      assert.equal(
        completed.filter((event) => event.type === 'user.custom_tool_result').length,
        1,
        'S2 retains one exact custom result',
      );
      assert.equal(upstream.requests.length, 2, 'S2 performs one initial and one continuation Provider request');
      assert.match(
        upstream.requests[1].system,
        /SYSTEM-CUSTOM-CONTEXT/u,
        'S2 installs the adjacent System before the resumed Provider request',
      );
      pass('custom-result + System commits atomically and resumes with the context');
    });

    console.log('E2E PASS: System batch placement and same-Run durability hold via TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
