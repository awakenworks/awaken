// Real-process, official-wire ACP permission coverage. An external ACP agent asks
// for a mutating bash call; the Session's normal policy parks the Run, and the
// Managed API resumes the exact durable ticket with an allow or deny decision.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function events(client, sessionId) {
  const found = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    found.push(event);
  }
  return found;
}

async function startAwaiting(client) {
  const session = await client.beta.sessions.create({
    agent: 'acp-agent',
    environment_id: 'env_local',
    betas: BETAS,
  });
  await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'request permission' }] }],
    betas: BETAS,
  });
  const observed = await events(client, session.id);
  const tool = observed.find((event) => event.type === 'agent.tool_use');
  assert.ok(tool, `ACP permission request projected as a tool use: ${JSON.stringify(observed)}`);
  assert.equal(tool.id, 'permission-call');
  assert.equal(tool.name, 'bash');
  assert.equal(tool.evaluated_permission, 'ask');
  const idle = observed.find((event) => event.type === 'session.status_idle');
  assert.equal(idle?.stop_reason?.type, 'requires_action');
  assert.deepEqual(idle.stop_reason.event_ids, ['permission-call']);
  return { session, tool };
}

async function decide(client, sessionId, toolId, result, denyMessage) {
  await client.beta.sessions.events.send(sessionId, {
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
  return events(client, sessionId);
}

function transcriptContains(observed, marker) {
  return observed
    .filter((event) => event.type === 'agent.message')
    .some((event) => JSON.stringify(event.content).includes(marker));
}

async function main() {
  await withServer('acp-permission', 38182, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const allowed = await startAwaiting(client);
    const allowEvents = await decide(client, allowed.session.id, allowed.tool.id, 'allow');
    assert.ok(transcriptContains(allowEvents, 'ACP-PERMISSION-ALLOWED'));
    assert.equal(allowEvents.at(-1)?.stop_reason?.type, 'end_turn');
    pass('ACP ask commits a durable ticket and an allow resumes the exact tool call');

    const denied = await startAwaiting(client);
    const denyEvents = await decide(client, denied.session.id, denied.tool.id, 'deny', 'policy denied');
    assert.ok(transcriptContains(denyEvents, 'ACP-PERMISSION-DENIED'));
    assert.equal(denyEvents.at(-1)?.stop_reason?.type, 'end_turn');
    pass('ACP deny selects the agent reject option and the Run still reaches a terminal boundary');

    console.log('E2E PASS: ACP permission wait + allow/deny resume over the Managed TS API.');
  });
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
