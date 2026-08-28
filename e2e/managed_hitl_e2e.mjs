// HITL end-to-end with the official Anthropic TypeScript SDK: a mutating tool
// parks for approval (requires_action + agent.tool_use{ask}); the client sends a
// `user.tool_confirmation`. Covers BOTH the allow path (tool runs, read-back
// succeeds) and the deny path (tool is blocked, the Run still completes).
//
// Uses the probe server (AWAKEN_MODEL_MODE=probe) with one explicit Session
// override: write probe.txt (asked), read it back (inherited allow), reply.
//
// Run: (from e2e/)  npm install && node managed_hitl_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { waitForSessionEventReceipt, withRealServer } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38102);
const BETAS = ['managed-agents-2026-04-01'];

async function newSession(client) {
  return client.beta.sessions.create({
    agent: {
      id: 'assistant',
      type: 'agent_with_overrides',
      tools: [{
        type: 'agent_toolset_20260401',
        configs: [{
          name: 'write',
          type: 'write',
          enabled: true,
          permission_policy: { type: 'always_ask' },
        }],
      }],
    },
    environment_id: 'env_local',
    betas: BETAS,
  });
}

async function main() {
  await withRealServer('probe', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    // Cause/effect graph: C0=the Session explicitly overrides write to
    // always_ask while read inherits the official default; C1=write becomes a
    // durably pending agent.tool_use; C2=the SDK sends allow for its qualified
    // Event id; C3=the SDK sends deny for that id. Effects: E1=requires_action
    // owns C1; E2=allow executes write, inherited-allow read proves its bytes,
    // and the Run ends; E3=deny blocks write but the Run still ends. Decision
    // table: H1(C0+C1+C2)->E1+E2; H2(C0+C1+C3)->E1+E3. Wrong family/id and
    // duplicate reply remain with protocol/error-path owners. Constraint: the
    // qualified tool-use Event id is the single decision authority and deny
    // cannot execute the pending mutation.

    // H1: allow path.
    {
      const session = await newSession(client);
      const initialReceipt = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'HELLO-ALLOW' }] }],
        betas: BETAS,
      });
      const initialReceiptId = initialReceipt.data[0]?.id;
      assert.equal(typeof initialReceiptId, 'string', 'H1 exact User Event receipt');
      let { events } = await waitForSessionEventReceipt(
        client,
        session.id,
        initialReceiptId,
        BETAS,
        ({ delta }) => {
          const pending = delta.find((event) => event.type === 'agent.tool_use');
          const idle = [...delta].reverse().find((event) => event.type === 'session.status_idle');
          return pending?.evaluated_permission === 'ask'
            && idle?.stop_reason?.type === 'requires_action'
            && idle.stop_reason.event_ids.includes(pending.id);
        },
        'H1 asked tool use to become durably pending',
      );
      const toolUse = events.find((e) => e.type === 'agent.tool_use');
      assert.ok(toolUse, 'expected agent.tool_use');
      assert.equal(toolUse.evaluated_permission, 'ask');
      const idle = events.find((e) => e.type === 'session.status_idle');
      assert.equal(idle.stop_reason.type, 'requires_action');
      assert.ok(idle.stop_reason.event_ids.includes(toolUse.id));

      const confirmation = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'allow' }],
        betas: BETAS,
      });
      const confirmationId = confirmation.data[0]?.id;
      assert.equal(typeof confirmationId, 'string', 'H1 exact confirmation receipt');
      ({ events } = await waitForSessionEventReceipt(
        client,
        session.id,
        confirmationId,
        BETAS,
        ({ delta }) => [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn'
          && JSON.stringify(delta.filter((event) => event.type === 'agent.tool_result').at(-1)?.content).includes('HELLO-ALLOW'),
        'H1 allow confirmation to execute the tool and settle the Run',
      ));
      const lastIdle = [...events].reverse().find((e) => e.type === 'session.status_idle');
      assert.equal(lastIdle.stop_reason.type, 'end_turn');
      const results = events.filter((e) => e.type === 'agent.tool_result');
      assert.ok(JSON.stringify(results.at(-1).content).includes('HELLO-ALLOW'), 'read-back after allow');
      console.log('  ok: allow -> tool runs, read-back succeeds');
    }

    // H2: deny path.
    {
      const session = await newSession(client);
      const initialReceipt = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'HELLO-DENY' }] }],
        betas: BETAS,
      });
      const initialReceiptId = initialReceipt.data[0]?.id;
      assert.equal(typeof initialReceiptId, 'string', 'H2 exact User Event receipt');
      let { events } = await waitForSessionEventReceipt(
        client,
        session.id,
        initialReceiptId,
        BETAS,
        ({ delta }) => {
          const pending = delta.find((event) => event.type === 'agent.tool_use');
          const idle = [...delta].reverse().find((event) => event.type === 'session.status_idle');
          return pending?.evaluated_permission === 'ask'
            && idle?.stop_reason?.type === 'requires_action'
            && idle.stop_reason.event_ids.includes(pending.id);
        },
        'H2 asked tool use to become durably pending',
      );
      const toolUse = events.find((e) => e.type === 'agent.tool_use');
      assert.equal(events.find((e) => e.type === 'session.status_idle').stop_reason.type, 'requires_action');

      const confirmation = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'deny', deny_message: 'not allowed' }],
        betas: BETAS,
      });
      const confirmationId = confirmation.data[0]?.id;
      assert.equal(typeof confirmationId, 'string', 'H2 exact confirmation receipt');
      ({ events } = await waitForSessionEventReceipt(
        client,
        session.id,
        confirmationId,
        BETAS,
        ({ delta }) => [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn',
        'H2 deny confirmation to block the tool and settle the Run',
      ));
      // The Run resumes and reaches its terminal wire reason even though the tool was denied.
      const lastIdle = [...events].reverse().find((e) => e.type === 'session.status_idle');
      assert.equal(lastIdle.stop_reason.type, 'end_turn');
      // The write was blocked, so the read-back does NOT contain the text.
      const results = events.filter((e) => e.type === 'agent.tool_result');
      assert.ok(!JSON.stringify(results.map((r) => r.content)).includes('HELLO-DENY'), 'deny should block the write');
      console.log('  ok: deny -> tool blocked, run still completes');
    }

    console.log('E2E PASS: HITL allow + deny round-trips via TS SDK.');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
