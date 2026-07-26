// Durable child-Thread projection recovery through the official Managed Agents
// TypeScript SDK. Runtime `RunDelegations` is the single relationship authority;
// a replacement server rebuilds its disposable Thread DTOs from that state.
//
// Cause graph:
//   completed delegation -> committed RunDelegations -> process loss
//   -> runtime relationship read port -> typed child Thread projection
//   missing/corrupt relationship -X-> invented child Thread
//
// Decision table:
// | durable relationship | process | projection |
// |---|---|---|
// | completed exact child | original | one idle child |
// | completed exact child | replacement | same id/agent/status/parent |
// | absent | either | primary only |

// Run: node e2e/managed_delegation_restart_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import {
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38237);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-delegation-restart-e2e-${process.pid}`;

async function listThreads(client, sessionId) {
  const threads = [];
  for await (const thread of client.beta.sessions.threads.list(sessionId, { betas: BETAS })) {
    threads.push(thread);
  }
  return threads;
}

async function listThreadEvents(client, sessionId, threadId) {
  const events = [];
  for await (const event of client.beta.sessions.threads.events.list(threadId, {
    session_id: sessionId,
    betas: BETAS,
  })) events.push(event);
  return events;
}

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const upstream = await startUpstream('delegating');
  const environment = {
    SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR,
    ...realServerEnv('delegating', upstream, { mode: 'delegate' }),
  };
  let server;
  try {
    server = spawnServer('delegate', PORT, environment);
    await waitForPort(PORT);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: server.baseUrl });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'research the answer' }] }],
      betas: BETAS,
    });
    const before = await listThreads(client, session.id);
    const original = before.find((thread) => thread.parent_thread_id !== null);
    assert.ok(original, 'the committed delegation creates one child Thread');
    assert.equal(original.status, 'idle');

    await stopServer(server.server);
    server = spawnServer('delegate', PORT, environment);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: server.baseUrl });
    const after = await listThreads(client, session.id);
    const restored = after.find((thread) => thread.parent_thread_id !== null);
    assert.ok(restored, 'the replacement process rebuilds the child projection');
    assert.equal(restored.id, original.id, 'exact durable child Run id is preserved');
    assert.equal(restored.agent.id, original.agent.id, 'exact delegate identity is preserved');
    assert.equal(restored.parent_thread_id, original.parent_thread_id);
    assert.equal(restored.status, 'idle', 'completed relationship restores as idle');
    const restoredEvents = await listThreadEvents(client, session.id, restored.id);
    assert.deepEqual(
      restoredEvents.map((event) => event.type),
      [
        'session.thread_status_running',
        'agent.thread_message_received',
        'agent.thread_message_sent',
        'session.thread_status_idle',
      ],
      'replacement process rebuilds the child-perspective history through the same projector',
    );
    assert.ok(
      restoredEvents[1].content.some((block) => block.text === 'do the research'),
      'the recovered child input comes from committed parent tool input',
    );
    assert.ok(
      restoredEvents[2].content.some((block) => block.text?.includes('researched: 42')),
      'the recovered child reply comes from the committed tool result',
    );
    pass('child Thread projection survived a real process restart from RunDelegations truth');
    console.log('E2E PASS: durable Managed child Thread projection and history recovery.');
  } finally {
    if (server) await stopServer(server.server);
    upstream.close();
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
