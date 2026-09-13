// The `evaluation` object on Agent tool-use events over the official Managed
// Agents client, for the policies the shipped evaluator does not decide.
//
// `managed_auto_permission_e2e` proves the two `auto` rows (allow, and ask on
// an indeterminate judgement). This file proves the fixed policies and the one
// absence the wire documents: an `always_ask` policy explains an ask as
// `{type: always_ask}`, an `always_allow` policy explains an allow as
// `{type: always_allow}`, and a tool the author never enabled is refused with
// `evaluated_permission: "deny"` and no `evaluation` at all — the server did
// not evaluate a policy, so it must not claim one.
//
// Deterministic, CI-safe (probe model: write then read), driven by the real
// binary and the official @anthropic-ai/sdk client.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import {
  spawnServer,
  stopServer,
  waitForPort,
  pass,
  startUpstream,
  realServerEnv,
  waitForSessionEventReceipt,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38292);
const STORE = `/tmp/awaken-permission-evaluation-${process.pid}`;

function agentWith(defaultPolicy, configs) {
  return {
    id: 'assistant',
    type: 'agent_with_overrides',
    tools: [{
      type: 'agent_toolset_20260401',
      default_config: { enabled: true, permission_policy: { type: defaultPolicy } },
      configs,
    }],
  };
}

async function createSession(client, agent) {
  const session = await client.beta.sessions.create({
    agent,
    environment_id: 'env_local',
    betas: BETAS,
  });
  const receipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'write then read probe.txt' }] }],
    betas: BETAS,
  });
  return { session, receiptId: receipt.data[0].id };
}

async function main() {
  fs.rmSync(STORE, { recursive: true, force: true });
  const upstream = await startUpstream('probe');
  const serverEnv = {
    SESSION_DEPLOYMENT_STORAGE_DIR: STORE,
    ...realServerEnv('probe', upstream),
  };
  let srv = null;
  try {
    srv = spawnServer('real', PORT, serverEnv);
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

    // Cause/effect graph: C1 the resolved policy for the call (always_ask,
    // always_allow, or none because the tool is disabled); C2 the call is
    // `write` (first) or `read` (second). Effects: E1 the top-level
    // evaluated_permission; E2 the evaluation object names the resolved policy;
    // E3 a refusal no policy explains carries no evaluation.
    //
    // | Rule | C1 (write) | C1 (read) | Effect |
    // |---|---|---|---|
    // | F1 | always_ask | always_allow | write: ask + {always_ask}; after approval read: allow + {always_allow} |
    // | F2 | disabled | always_allow | write: deny, no evaluation; read: allow + {always_allow} |

    // F1: the toolset asks by default; `read` is allowed outright.
    {
      const { session, receiptId } = await createSession(
        client,
        agentWith('always_ask', [{ name: 'read', permission_policy: { type: 'always_allow' } }]),
      );
      const gated = await waitForSessionEventReceipt(
        client, session.id, receiptId, BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.tool_use')
          && [...delta].reverse().find((event) => event.type === 'session.status_idle')
            ?.stop_reason?.type === 'requires_action',
        'F1 the always_ask policy to pause the write',
      );
      const write = gated.delta.find((event) => event.type === 'agent.tool_use');
      assert.equal(write.name, 'write', 'F1 the gated call is the write');
      assert.equal(write.evaluated_permission, 'ask', 'F1/E1');
      assert.deepEqual(write.evaluation, { type: 'always_ask' }, 'F1/E2');

      const approval = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: write.id, result: 'allow' }],
        betas: BETAS,
      });
      const finished = await waitForSessionEventReceipt(
        client, session.id, approval.data[0].id, BETAS,
        ({ delta }) => [...delta].reverse().find((event) => event.type === 'session.status_idle')
          ?.stop_reason?.type === 'end_turn',
        'F1 the turn to finish after approval',
      );
      const read = finished.delta.find((event) => event.type === 'agent.tool_use' && event.name === 'read');
      assert.ok(read, 'F1 the read ran');
      assert.equal(read.evaluated_permission, 'allow', 'F1/E1');
      assert.deepEqual(read.evaluation, { type: 'always_allow' }, 'F1/E2');
      pass('always_ask and always_allow each explain the outcome they produced');
    }

    // F2: `write` is not enabled at all; the refusal names no policy.
    {
      const { session, receiptId } = await createSession(
        client,
        agentWith('always_allow', [{ name: 'write', enabled: false }]),
      );
      const finished = await waitForSessionEventReceipt(
        client, session.id, receiptId, BETAS,
        ({ delta }) => [...delta].reverse().find((event) => event.type === 'session.status_idle')
          ?.stop_reason?.type === 'end_turn',
        'F2 the turn to finish without pausing',
      );
      const uses = finished.delta.filter((event) => event.type === 'agent.tool_use');
      const write = uses.find((event) => event.name === 'write');
      const read = uses.find((event) => event.name === 'read');
      assert.ok(write && read, `F2 both calls are visible: ${uses.map((event) => event.name)}`);
      assert.equal(write.evaluated_permission, 'deny', 'F2/E1 a disabled tool is refused');
      assert.equal(write.evaluation, undefined, 'F2/E3 no policy was evaluated, so none is claimed');
      assert.equal(read.evaluated_permission, 'allow', 'F2/E1');
      assert.deepEqual(read.evaluation, { type: 'always_allow' }, 'F2/E2');
      pass('a tool the author never enabled is refused without a claimed evaluation');
    }
  } finally {
    if (srv) {
      await stopServer(srv.server);
    }
    await upstream.close();
    fs.rmSync(STORE, { recursive: true, force: true });
  }
}

await main();
