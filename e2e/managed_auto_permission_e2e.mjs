// The `auto` permission policy over the official Managed Agents client.
//
// A session declares `permission_policy: {type: 'auto'}` for the whole Agent
// toolset and states no fixed answer for any tool. The probe model then calls
// `write` and, after the turn resumes, `read` — one turn that exercises both
// outcomes the shipped evaluator can produce, and proves the resolution order:
// `auto` decides only where the author stated nothing, and a call it cannot
// judge reaches a person instead of being refused or waved through.
//
// Deterministic, CI-safe (probe model), driven by the real binary and the
// official @anthropic-ai/sdk client.

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
const PORT = Number(process.env.E2E_PORT ?? 38291);
const STORE = `/tmp/awaken-auto-permission-${process.pid}`;

/// An Agent whose whole toolset defers to the platform. Naming no per-tool
/// policy is the point: every decision in this scenario comes from `auto`.
function autoToolsetAgent(agentId = 'assistant') {
  return {
    id: agentId,
    type: 'agent_with_overrides',
    tools: [{
      type: 'agent_toolset_20260401',
      default_config: { enabled: true, permission_policy: { type: 'auto' } },
    }],
  };
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

    // Cause/effect graph: C1 the toolset defers every tool to `auto`; C2 the
    // call modifies (`write`); C3 the call only observes (`read`). Effects:
    // E1 a modifying call the evaluator cannot judge reaches a person, reported
    // as `ask` — never refused and never waved through; E2 an observing call
    // runs unattended, reported as `allow`; E3 the authored policy round-trips
    // as `auto` rather than as whatever it resolved to for one call.
    //
    // | Rule | C2 | C3 | Effect |
    // |---|---|---|---|
    // | A1 | T | F | E1 ask, session idles requires_action |
    // | A2 | F | T | E2 allow, executes without a confirmation |
    const session = await client.beta.sessions.create({
      agent: autoToolsetAgent(),
      environment_id: 'env_local',
      betas: BETAS,
    });

    const retained = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    const toolset = retained.agent.tools.find((tool) => tool.type === 'agent_toolset_20260401');
    assert.equal(
      toolset?.default_config?.permission_policy?.type,
      'auto',
      'A3/E3 the authored policy round-trips as `auto`',
    );
    pass('an `auto` toolset policy round-trips through the official client');

    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'write then read probe.txt' }] }],
      betas: BETAS,
    });
    const receiptId = receipt.data[0]?.id;
    assert.equal(typeof receiptId, 'string', 'an exact User Event receipt');

    const gated = await waitForSessionEventReceipt(
      client,
      session.id,
      receiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.tool_use')
        && [...delta].reverse().find((event) => event.type === 'session.status_idle')
          ?.stop_reason?.type === 'requires_action',
      'A1 the modifying call to reach a person',
    );
    const modifying = gated.delta.find((event) => event.type === 'agent.tool_use');
    assert.equal(modifying.name, 'write', 'A1 the gated call is the modifying one');
    assert.equal(
      modifying.evaluated_permission,
      'ask',
      'A1/E1 a call `auto` cannot judge is reported as ask, not allow and not deny',
    );
    pass('auto sends a modifying call to a person instead of deciding it');

    const approval = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.tool_confirmation', tool_use_id: modifying.id, result: 'allow' }],
      betas: BETAS,
    });
    const approvalId = approval.data[0]?.id;
    assert.equal(typeof approvalId, 'string', 'an exact confirmation receipt');

    const finished = await waitForSessionEventReceipt(
      client,
      session.id,
      approvalId,
      BETAS,
      ({ delta }) => [...delta].reverse().find((event) => event.type === 'session.status_idle')
        ?.stop_reason?.type === 'end_turn',
      'A2 the turn to finish after approval',
    );
    const observing = finished.delta.find(
      (event) => event.type === 'agent.tool_use' && event.name === 'read',
    );
    assert.ok(observing, 'A2 the observing call happened');
    assert.equal(
      observing.evaluated_permission,
      'allow',
      'A2/E2 an observing call runs unattended under auto',
    );
    assert.ok(
      !finished.delta.some(
        (event) => event.type === 'agent.tool_use'
          && event.name === 'read'
          && event.evaluated_permission === 'ask',
      ),
      'A2/E2 the observing call needed no confirmation',
    );
    pass('auto runs an observing call unattended in the same turn');
  } finally {
    if (srv) {
      await stopServer(srv.server);
    }
    await upstream.close();
    fs.rmSync(STORE, { recursive: true, force: true });
  }
}

await main();
