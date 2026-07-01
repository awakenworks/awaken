// Comprehensive Managed Agents e2e with the official Anthropic TypeScript SDK
// (echo model). Covers: session create + retrieve, single and multi-turn
// messages, event list, SSE stream (events.stream), and error handling.
//
// Run: (from e2e/)  npm install && node managed_e2e.mjs

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

async function listTypes(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function sendMessage(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

async function main() {
  const server = spawn('cargo', ['run', '--quiet', '-p', 'awaken-server-local'], {
    cwd: REPO_ROOT,
    env: { ...process.env, AWAKEN_HTTP_ADDR: ADDR },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  server.on('exit', (code) => {
    if (code !== null && code !== 0) { console.error(`server exited early: ${code}`); process.exit(1); }
  });

  try {
    await waitForPort(PORT, 180_000);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${ADDR}` });

    // --- create + retrieve ---
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    assert.equal(session.type, 'session');
    assert.equal(session.status, 'idle');
    assert.ok(session.id.startsWith('sesn_'));

    const retrieved = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.equal(retrieved.id, session.id);
    assert.equal(retrieved.agent.id, 'assistant');
    console.log('  ok: create + retrieve');

    // --- single message ---
    await sendMessage(client, session.id, 'hi there');
    let events = await listTypes(client, session.id);
    assert.deepEqual(events.map((e) => e.type), ['agent.message', 'session.status_idle']);
    assert.equal(events.find((e) => e.type === 'agent.message').content[0].text, 'Echo: hi there');
    assert.equal(events.find((e) => e.type === 'session.status_idle').stop_reason.type, 'end_turn');
    console.log('  ok: single message + list');

    // --- multi-turn conversation ---
    await sendMessage(client, session.id, 'second');
    events = await listTypes(client, session.id);
    const messages = events.filter((e) => e.type === 'agent.message').map((e) => e.content[0].text);
    assert.deepEqual(messages, ['Echo: hi there', 'Echo: second']);
    console.log('  ok: multi-turn conversation');

    // --- SSE stream (events.stream) ---
    const stream = await client.beta.sessions.events.stream(session.id, { betas: BETAS });
    const streamedTypes = [];
    for await (const ev of stream) streamedTypes.push(ev.type);
    assert.ok(streamedTypes.includes('agent.message'), `stream types: ${streamedTypes}`);
    assert.ok(streamedTypes.includes('session.status_idle'), `stream types: ${streamedTypes}`);
    console.log('  ok: SSE stream via events.stream');

    // --- errors: unknown session ---
    await assert.rejects(
      () => client.beta.sessions.retrieve('sesn_does_not_exist', { betas: BETAS }),
      (err) => { assert.equal(err.status, 404); return true; },
    );
    console.log('  ok: unknown session -> 404');

    console.log('E2E PASS: Managed Agents lifecycle/messages/stream/errors via TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    server.kill('SIGINT');
  }
}

main();
