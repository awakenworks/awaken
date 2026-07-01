// HITL end-to-end test with the official Anthropic TypeScript SDK: a mutating
// tool parks for approval (`session.status_idle{requires_action}` +
// `agent.tool_use{evaluated_permission:"ask"}`), the client sends a
// `user.tool_confirmation`, and the run resumes to `end_turn`.
//
// Uses the probe server (AWAKEN_MODEL_MODE=probe): write probe.txt (asked ->
// parks), read it back (allowed), reply.
//
// Run: (from e2e/)  npm install && node managed_hitl_e2e.mjs

import assert from 'node:assert/strict';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38102);
const ADDR = `127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];

function waitForPort(port, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      const sock = net.createConnection({ port, host: '127.0.0.1' });
      sock.once('connect', () => { sock.destroy(); resolve(); });
      sock.once('error', () => {
        sock.destroy();
        if (Date.now() > deadline) reject(new Error(`server did not listen on ${port}`));
        else setTimeout(attempt, 200);
      });
    };
    attempt();
  });
}

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(ev);
  }
  return events;
}

async function main() {
  const server = spawn('cargo', ['run', '--quiet', '-p', 'awaken-server-local'], {
    cwd: REPO_ROOT,
    env: { ...process.env, AWAKEN_HTTP_ADDR: ADDR, AWAKEN_MODEL_MODE: 'probe' },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  server.on('exit', (code) => {
    if (code !== null && code !== 0) {
      console.error(`server exited early with code ${code}`);
      process.exit(1);
    }
  });

  try {
    await waitForPort(PORT, 180_000);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${ADDR}` });

    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });

    // 1. A message -> the mutating tool parks for approval.
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'HELLO-SDK-HITL' }] }],
      betas: BETAS,
    });

    let events = await listEvents(client, session.id);
    const toolUse = events.find((e) => e.type === 'agent.tool_use');
    assert.ok(toolUse, `expected an agent.tool_use, got: ${events.map((e) => e.type)}`);
    assert.equal(toolUse.evaluated_permission, 'ask');
    const idle = events.find((e) => e.type === 'session.status_idle');
    assert.equal(idle.stop_reason.type, 'requires_action');
    assert.ok(idle.stop_reason.event_ids.includes(toolUse.id), 'requires_action references the tool_use');

    // 2. Confirm the tool -> the run resumes and completes.
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'allow' }],
      betas: BETAS,
    });

    events = await listEvents(client, session.id);
    const lastIdle = [...events].reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(lastIdle.stop_reason.type, 'end_turn');
    const results = events.filter((e) => e.type === 'agent.tool_result');
    const readBack = results[results.length - 1];
    assert.ok(
      JSON.stringify(readBack.content).includes('HELLO-SDK-HITL'),
      `read-back should contain the written text, got: ${JSON.stringify(readBack.content)}`,
    );

    console.log('E2E PASS: Anthropic TS SDK drove the HITL confirmation round-trip.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    server.kill('SIGINT');
  }
}

main();
