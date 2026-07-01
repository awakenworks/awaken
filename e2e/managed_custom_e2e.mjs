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
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38104);
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
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function main() {
  const server = spawn('cargo', ['run', '--quiet', '-p', 'awaken-server-local'], {
    cwd: REPO_ROOT,
    env: { ...process.env, AWAKEN_HTTP_ADDR: ADDR, AWAKEN_MODEL_MODE: 'custom' },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  server.on('exit', (code) => {
    if (code !== null && code !== 0) { console.error(`server exited early: ${code}`); process.exit(1); }
  });

  try {
    await waitForPort(PORT, 180_000);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${ADDR}` });

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
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    server.kill('SIGINT');
  }
}

main();
