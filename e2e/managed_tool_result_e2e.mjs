// The generic `user.tool_result` inbound event (SDK: sessions.events.send with a
// `user.tool_result` param). A client tool call parks the run
// (`agent.custom_tool_use` + `requires_action`); the client returns the result via
// `user.tool_result` keyed by the parked `tool_use_id` (rather than
// `user.custom_tool_result`'s `custom_tool_use_id`), and the run resumes. Proves
// the repo accepts the SDK's generic tool-result event, not only the custom one.
//
// Run: (from e2e/)  node managed_tool_result_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38236);
const BETAS = ['managed-agents-2026-04-01'];

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function main() {
  await withScenarioServer('custom', 'custom', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });

    // Message -> the client tool call parks.
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'solve it' }] }],
      betas: BETAS,
    });
    let events = await listEvents(client, session.id);
    const toolUse = events.find((e) => e.type === 'agent.custom_tool_use');
    assert.ok(toolUse, `expected agent.custom_tool_use, got: ${events.map((e) => e.type)}`);
    const idle = events.find((e) => e.type === 'session.status_idle');
    assert.equal(idle.stop_reason.type, 'requires_action');
    assert.ok(idle.stop_reason.event_ids.includes(toolUse.id), 'the parked tool id is in event_ids');

    // Return the result via the GENERIC user.tool_result (keyed by tool_use_id).
    await client.beta.sessions.events.send(session.id, {
      events: [
        { type: 'user.tool_result', tool_use_id: toolUse.id, content: [{ type: 'text', text: '42' }] },
      ],
      betas: BETAS,
    });
    events = await listEvents(client, session.id);
    const messages = events.filter((e) => e.type === 'agent.message').map((e) => e.content[0].text);
    assert.ok(messages.some((m) => m.includes('42')), `user.tool_result reached the model: ${messages}`);
    const lastIdle = [...events].reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(lastIdle.stop_reason.type, 'end_turn');

    console.log('E2E PASS: generic user.tool_result round-trips a parked tool via TS SDK.');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
