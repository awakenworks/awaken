// Custom (client-executed) tool end-to-end with the official Anthropic TS SDK:
// the model calls a client tool -> the run parks as `agent.custom_tool_use` +
// `requires_action`; the client executes it and returns
// `user.custom_tool_result`; the run resumes and the model uses the result.
//
// Uses the custom server (AWAKEN_MODEL_MODE=custom): a `submit_answer` client
// tool (model-visible, no server-side executable).
//
// Run: (from e2e/)  npm install && node managed_custom_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38104);
const BETAS = ['managed-agents-2026-04-01'];

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function main() {
  // The `custom` host config (the `submit_answer` client tool) with the model on the
  // real wire (the `custom` behavior: call the tool, then reply with its result).
  await withScenarioServer('custom', 'custom', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

    // Message -> the client tool call parks.
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'solve it' }] }],
      betas: BETAS,
    });
    let events = await listEvents(client, session.id);
    const customUse = events.find((e) => e.type === 'agent.custom_tool_use');
    assert.ok(customUse, `expected agent.custom_tool_use, got: ${events.map((e) => e.type)}`);
    assert.equal(customUse.name, 'submit_answer');
    const idle = events.find((e) => e.type === 'session.status_idle');
    assert.equal(idle.stop_reason.type, 'requires_action');
    assert.ok(idle.stop_reason.event_ids.includes(customUse.id));

    // Client runs the tool and returns the result.
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.custom_tool_result', custom_tool_use_id: customUse.id, content: [{ type: 'text', text: '42' }] }],
      betas: BETAS,
    });
    events = await listEvents(client, session.id);
    const messages = events.filter((e) => e.type === 'agent.message').map((e) => e.content[0].text);
    assert.ok(messages.some((m) => m.includes('42')), `client result reached the model: ${messages}`);
    const lastIdle = [...events].reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(lastIdle.stop_reason.type, 'end_turn');

    console.log('E2E PASS: custom (client-executed) tool round-trip via TS SDK.');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
