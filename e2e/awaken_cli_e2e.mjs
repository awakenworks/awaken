// Deployment-agnostic e2e for the aggregated `awaken` command (crate awaken-cli).
//
// `awaken` is the single binary that subsumes awaken-server-local and
// awaken-standalone: configuration (AWAKEN_ROLE + the deployment axes) decides the
// deployment. This test exercises the default **Serve** role — the single-machine
// all-in-one — black-box over real HTTP with the official Anthropic SDK, proving the
// aggregated command boots, seeds its keys, enforces the session guard, and drives a
// full agent turn. The Serve role reuses `awaken_standalone::build` verbatim, so the
// broader protocol/durability surface is covered by standalone_e2e; this asserts the
// aggregation wiring itself is live.
//
// Run: (from e2e/)  npm install && node awaken_cli_e2e.mjs

import assert from 'node:assert/strict';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import fs from 'node:fs';
import readline from 'node:readline';
import { spawn, execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38411);
const BETAS = ['managed-agents-2026-04-01'];
const HELLO = 'Hello from awaken-standalone.'; // reused HelloModel reply
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function awakenBin() {
  const out = execSync(
    'cargo build --quiet --message-format=json -p awaken-cli --bin awaken',
    { cwd: REPO_ROOT, maxBuffer: 64 * 1024 * 1024 },
  ).toString();
  for (const line of out.split('\n')) {
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.executable && msg.target?.name === 'awaken') return msg.executable;
  }
  throw new Error('could not resolve the awaken binary path');
}

function waitForPort(port, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      const sock = net.createConnection({ port, host: '127.0.0.1' });
      sock.once('connect', () => {
        sock.destroy();
        resolve();
      });
      sock.once('error', () => {
        sock.destroy();
        if (Date.now() > deadline) reject(new Error(`server did not listen on ${port}`));
        else setTimeout(attempt, 200);
      });
    };
    attempt();
  });
}

function startAwaken(bin, port, extraEnv = {}) {
  const server = spawn(bin, {
    env: { ...process.env, AWAKEN_HTTP_ADDR: `127.0.0.1:${port}`, ...extraEnv },
    stdio: ['ignore', 'inherit', 'pipe'],
  });
  const keys = {};
  readline.createInterface({ input: server.stderr }).on('line', (line) => {
    process.stderr.write(`${line}\n`);
    const admin = line.match(/admin key:\s+(sk-awaken-\S+)/);
    const api = line.match(/api key:\s+(sk-awaken-\S+)/);
    if (admin) keys.admin = admin[1];
    if (api) keys.api = api[1];
  });
  const stop = () =>
    new Promise((resolve) => {
      if (server.exitCode !== null) return resolve();
      server.on('exit', () => resolve());
      server.kill('SIGINT');
    });
  return { server, keys, baseUrl: `http://127.0.0.1:${port}`, stop };
}

async function ready(handle, port) {
  await waitForPort(port);
  for (let i = 0; i < 200 && !handle.keys.api; i++) await sleep(50);
  assert.ok(handle.keys.api?.startsWith('sk-awaken-'), 'captured the seeded api key from the banner');
}

function sdk(baseUrl, token) {
  return new Anthropic({ apiKey: null, authToken: token, baseURL: baseUrl });
}

// One full agent turn: create a session, send a user message, drain the events.
async function turn(client, text) {
  const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
  await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const events = [];
  for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
  return events.filter((e) => e.type === 'agent.message').map((e) => e.content[0].text);
}

async function main() {
  const bin = awakenBin();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-cli-e2e-'));
  const h = startAwaken(bin, PORT, { AWAKEN_STORAGE_DIR: dir });
  try {
    await ready(h, PORT);
    console.log('ok: aggregated `awaken` command booted in the default Serve role');

    // The session-axis guard is live: no credential → 401.
    const anon = await fetch(`${h.baseUrl}/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
      body: JSON.stringify({ agent: 'assistant' }),
    });
    assert.equal(anon.status, 401, '/v1/sessions requires a credential');
    console.log('ok: session guard rejects the anonymous caller');

    // A full agent turn over the aggregated command.
    const replies = await turn(sdk(h.baseUrl, h.keys.api), 'hello awaken');
    assert.ok(replies.some((t) => t.includes(HELLO)), `turn replies: ${JSON.stringify(replies)}`);
    console.log('ok: full agent turn on the aggregated command');
  } finally {
    await h.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
  console.log('\nawaken_cli_e2e: PASS');
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
