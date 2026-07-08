// Multi-agent delegation end-to-end with the official Anthropic TS SDK: the main
// agent calls the built-in `agent_run` tool, which the server backs with an
// in-process sub-run (a `researcher` delegate). The delegate's result flows back
// and the main agent reports it — all within one turn (agent_run runs inline).
//
// Uses the delegation server (AWAKEN_MODEL_MODE=delegate): roster = {researcher};
// `ghost` is intentionally absent so the fail-closed path can be shown too.
//
// Run: (from e2e/)  npm install && node managed_delegation_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38105);
const BETAS = ['managed-agents-2026-04-01'];

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function turn(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  return listEvents(client, sessionId);
}

function messages(events) {
  return events.filter((e) => e.type === 'agent.message').map((e) => e.content[0].text);
}

async function main() {
  await withScenarioServer('delegate', 'delegating', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    // Happy path: delegate to `researcher`, whose result flows back.
    const ok = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    const okEvents = await turn(client, ok.id, 'research the answer');
    const toolUse = okEvents.find((e) => e.type === 'agent.tool_use');
    assert.ok(toolUse && toolUse.name === 'agent_run', `expected agent_run tool_use, got: ${okEvents.map((e) => e.type)}`);
    assert.ok(
      messages(okEvents).some((m) => m.includes('delegate said: researched: 42')),
      `delegate result reached the main agent: ${messages(okEvents)}`,
    );
    const okIdle = [...okEvents].reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(okIdle.stop_reason.type, 'end_turn');

    // D4: the delegate call spawned a subagent child thread — announced by
    // `session.thread_created` and enumerable via the threads API.
    const created = okEvents.find((e) => e.type === 'session.thread_created');
    assert.ok(created, `expected session.thread_created, got: ${okEvents.map((e) => e.type)}`);
    assert.equal(created.agent_name, 'researcher');

    const threads = [];
    for await (const t of client.beta.sessions.threads.list(ok.id, { betas: BETAS })) threads.push(t);
    assert.equal(threads.length, 2, `primary + researcher child: ${threads.map((t) => t.id)}`);
    const primary = threads.find((t) => t.parent_thread_id === null);
    const child = threads.find((t) => t.id === created.session_thread_id);
    assert.ok(child, 'the created child thread is enumerated');
    assert.equal(child.parent_thread_id, primary.id, 'child links to the primary thread');
    assert.equal(child.agent.name, 'researcher');

    const gotChild = await client.beta.sessions.threads.retrieve(child.id, {
      session_id: ok.id,
      betas: BETAS,
    });
    assert.equal(gotChild.id, child.id, 'the child thread is retrievable');

    // The child thread's inline run is bracketed: created → running → idle.
    const childEvents = okEvents.filter((e) => e.session_thread_id === child.id);
    assert.deepEqual(
      childEvents.map((e) => e.type),
      ['session.thread_created', 'session.thread_status_running', 'session.thread_status_idle'],
      `child thread lifecycle: ${childEvents.map((e) => e.type)}`,
    );
    const childIdle = childEvents.find((e) => e.type === 'session.thread_status_idle');
    assert.equal(childIdle.stop_reason.type, 'end_turn');

    // Fail closed: `ghost` is not in the roster; no sub-run runs.
    const bad = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    const badEvents = await turn(client, bad.id, 'use the ghost agent');
    const badText = badEvents
      .filter((e) => e.type === 'agent.message' || e.type === 'agent.tool_result')
      .map((e) => e.content?.[0]?.text ?? '')
      .join(' | ');
    assert.ok(badText.includes('roster'), `roster rejection surfaced: ${badText}`);
    assert.ok(!badText.includes('researched: 42'), `no sub-run output leaked: ${badText}`);

    console.log('E2E PASS: multi-agent delegation (happy + fail-closed) via TS SDK.');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
