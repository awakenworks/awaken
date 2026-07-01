// End-to-end test: drive `awaken-server-local` with the official Anthropic
// TypeScript SDK (@anthropic-ai/sdk), proving the server is wire-compatible with
// the Managed Agents runtime protocol. Spawns the server, creates a session,
// sends a `user.message`, and lists the projected events via the SDK.
//
// Run: (from e2e/)  npm install && npm test

import assert from 'node:assert/strict';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38099);
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

async function main() {
  // The server compiles on first run; allow generous startup time.
  const server = spawn('cargo', ['run', '--quiet', '-p', 'awaken-server-local'], {
    cwd: REPO_ROOT,
    env: { ...process.env, AWAKEN_HTTP_ADDR: ADDR },
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

    // 1. Create a session (SDK -> POST /v1/sessions).
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    assert.equal(session.type, 'session');
    assert.equal(session.status, 'idle');
    assert.ok(session.id.startsWith('sesn_'), `session id: ${session.id}`);

    // 2. Send a user.message (SDK -> POST /v1/sessions/{id}/events).
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'hi there' }] }],
      betas: BETAS,
    });
    assert.equal(receipt.data[0].type, 'user.message');

    // 3. List the projected events (SDK -> GET /v1/sessions/{id}/events, paginated).
    const events = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
      events.push(ev);
    }
    const types = events.map((e) => e.type);
    assert.ok(types.includes('agent.message'), `types: ${types}`);
    assert.ok(types.includes('session.status_idle'), `types: ${types}`);

    const agentMsg = events.find((e) => e.type === 'agent.message');
    assert.equal(agentMsg.content[0].text, 'Echo: hi there');

    const idle = events.find((e) => e.type === 'session.status_idle');
    assert.equal(idle.stop_reason.type, 'end_turn');

    console.log('E2E PASS: Anthropic TS SDK drove the Managed Agents surface end-to-end.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    server.kill('SIGINT');
  }
}

main();
