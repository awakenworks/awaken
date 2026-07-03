// Skills end-to-end (ADR-0036) via the Anthropic TS SDK.
//
// The `skills` server offers two skills fronted by the single `Skill` tool plus
// `list_skills`. The model discovers them (`list_skills` → catalog), activates one
// (`Skill { skill: "greet" }` → the skill's instructions), then replies with those
// instructions — so we assert the whole discover → activate → use flow over HTTP:
// the skill catalog is delivered, activation returns the skill body, and the reply
// reflects it.
//
// Run: (from e2e/)  node managed_skills_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38183);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

const listEvents = async (sessionId) => {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
};

async function main() {
  const { server } = spawnServer('skills', PORT);
  await waitForPort(PORT);
  try {
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'please discover and use a skill' }] }],
      betas: BETAS,
    });
    const events = await listEvents(session.id);

    // The model discovered skills and then activated one — both tool calls present.
    const toolNames = events.filter((e) => e.type === 'agent.tool_use').map((e) => e.name);
    assert.ok(toolNames.includes('list_skills'), 'the model discovered skills via list_skills');
    assert.ok(toolNames.includes('Skill'), 'the model activated a skill via the Skill tool');
    pass('discover (list_skills) → activate (Skill) tool calls both present');

    // The delivered catalog listed both offered skills.
    const results = JSON.stringify(events.filter((e) => e.type === 'agent.tool_result').map((r) => r.content));
    assert.ok(results.includes('greet') && results.includes('review'), 'the catalog listed the offered skills');
    pass('the skill catalog was delivered with both offered skills');

    // The reply reflects the activated skill's instructions.
    const reply = JSON.stringify(events.filter((e) => e.type === 'agent.message').map((m) => m.content));
    assert.ok(reply.includes('GREETING-FROM-SKILL'), "the run used the activated skill's instructions");
    pass('the activated skill instructions reached the reply (discover → activate → use)');

    console.log('E2E PASS: skill discovery → activation → use via TS SDK (ADR-0036).');
  } finally {
    await stopServer(server);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
