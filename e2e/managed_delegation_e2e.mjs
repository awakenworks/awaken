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
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38105);
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
  const server = spawn('cargo', ['run', '--quiet', '-p', 'awaken-server-local'], {
    cwd: REPO_ROOT,
    env: { ...process.env, AWAKEN_HTTP_ADDR: ADDR, AWAKEN_MODEL_MODE: 'delegate' },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  server.on('exit', (code) => {
    if (code !== null && code !== 0) { console.error(`server exited early: ${code}`); process.exit(1); }
  });

  try {
    await waitForPort(PORT, 180_000);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${ADDR}` });

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
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    server.kill('SIGINT');
  }
}

main();
