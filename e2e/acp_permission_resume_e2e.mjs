// Real-process, official-wire ACP permission coverage. An external ACP agent asks
// for a mutating bash call; the Session's normal policy parks the Run, and the
// Managed API resumes the exact durable ticket with an allow or deny decision.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass, waitForSessionEventReceipt } from './harness.mjs';
import { startAcpPermissionAwait } from './fixtures/acp_permission_await.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function decide(client, sessionId, toolId, result, expectedMarker, denyMessage) {
  // Resume decision rules: D1 listed qualified tool id + allow/deny => one exact
  // confirmation receipt; D2 receipt processed + matching terminal marker and
  // end_turn => return; D3 raw/stale/wrong-family id => synchronous rejection.
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [
      {
        type: 'user.tool_confirmation',
        tool_use_id: toolId,
        result,
        ...(denyMessage ? { deny_message: denyMessage } : {}),
      },
    ],
    betas: BETAS,
  });
  const receiptId = receipt.data?.[0]?.id;
  assert.equal(typeof receiptId, 'string', 'official SDK returns the exact confirmation receipt');
  const { delta } = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta: observed }) => transcriptContains(observed, expectedMarker)
        && observed.some((event) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'end_turn'),
    `ACP ${result} decision to commit its terminal Agent reply`,
  );
  return delta;
}

function transcriptContains(observed, marker) {
  return observed
    .filter((event) => event.type === 'agent.message')
    .some((event) => JSON.stringify(event.content).includes(marker));
}

async function main() {
  // Test design (allow/deny arms). Causes: C1=ACP parks on one qualified
  // agent.tool_use; C2=the client confirms allow; C3=the client confirms deny.
  // Effects: E1=requires_action names exactly that public Event; E2=C2 resumes
  // the same call and commits ALLOWED; E3=C3 selects rejection and commits
  // DENIED; both arms terminate at end_turn. Constraints/invariant: the raw ACP
  // call id is never client authority and each decision is single-use.
  // Decision rules: P1=C1+C2=>E1+E2; P2=C1+C3=>E1+E3.
  await withServer('acp-permission', 38182, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const allowed = await startAcpPermissionAwait(client, BETAS);
    const allowEvents = await decide(
      client,
      allowed.session.id,
      allowed.tool.id,
      'allow',
      'ACP-PERMISSION-ALLOWED',
    );
    assert.ok(transcriptContains(allowEvents, 'ACP-PERMISSION-ALLOWED'));
    assert.equal(
      allowEvents.findLast((event) => event.type === 'session.status_idle')?.stop_reason?.type,
      'end_turn',
    );
    pass('ACP ask commits a durable ticket and an allow resumes the exact tool call');

    const denied = await startAcpPermissionAwait(client, BETAS);
    const denyEvents = await decide(
      client,
      denied.session.id,
      denied.tool.id,
      'deny',
      'ACP-PERMISSION-DENIED',
      'policy denied',
    );
    assert.ok(transcriptContains(denyEvents, 'ACP-PERMISSION-DENIED'));
    assert.equal(
      denyEvents.findLast((event) => event.type === 'session.status_idle')?.stop_reason?.type,
      'end_turn',
    );
    pass('ACP deny selects the agent reject option and the Run still reaches a terminal boundary');

    console.log('E2E PASS: ACP permission wait + allow/deny resume over the Managed TS API.');
  });
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
