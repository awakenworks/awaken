// Deployment-agnostic e2e for the OPEN single-machine binary (awaken-standalone).
//
// Black-box over real HTTP with the official Anthropic TypeScript SDK, so the same
// assertions hold against any deployment form that speaks the Managed Agents API
// (standalone / private / cloud) — enforcement, the agent loop, and durability are
// properties of the shared open runtime, not of a deployment. Specific to the
// standalone: it seeds a singleton tenant + two keys at boot (printed to stderr)
// and serves the whole open protocol surface behind the session-axis guard.
//
// Run: (from e2e/)  npm install && node standalone_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { spawn, execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const BASE_PORT = Number(process.env.E2E_PORT ?? 38311);
const BETAS = ['managed-agents-2026-04-01'];
const HELLO = 'Hello from awaken-standalone.';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function standaloneBin() {
  const out = execSync(
    'cargo build --quiet --message-format=json -p awaken-standalone --bin awaken-standalone',
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
    if (msg.executable && msg.target?.name === 'awaken-standalone') return msg.executable;
  }
  throw new Error('could not resolve the awaken-standalone binary path');
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

// Spawn the standalone on `port`, capturing the seeded keys from its stderr banner.
// Returns the base URL, the (async-populated) keys, and a `stop()` that resolves
// once the process has actually exited (so the storage dir is flushed for a restart).
function startStandalone(bin, port, extraEnv = {}) {
  const server = spawn(bin, {
    env: { ...process.env, AWAKEN_STANDALONE_ADDR: `127.0.0.1:${port}`, ...extraEnv },
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

function client(baseUrl, apiToken, { header = 'bearer' } = {}) {
  return header === 'x-api-key'
    ? new Anthropic({ apiKey: apiToken, baseURL: baseUrl })
    : new Anthropic({ apiKey: null, authToken: apiToken, baseURL: baseUrl });
}

// Create a session, send one user message, and return the agent's reply texts.
async function converse(sdk) {
  const session = await sdk.beta.sessions.create({ agent: 'assistant', betas: BETAS });
  assert.ok(session.id.startsWith('sesn_'), `session id: ${session.id}`);
  await sdk.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'hi' }] }],
    betas: BETAS,
  });
  const events = [];
  for await (const ev of sdk.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
  return {
    id: session.id,
    replies: events.filter((e) => e.type === 'agent.message').map((e) => e.content[0].text),
  };
}

// ── Phase 1: enforcement + the agent loop, ephemeral (no storage dir) ─────────
async function enforcementAndAgentLoop(bin) {
  const port = BASE_PORT;
  const h = startStandalone(bin, port);
  try {
    await ready(h, port);
    assert.notEqual(h.keys.api, h.keys.admin, 'the two seeded keys are distinct');

    // Every session call needs a credential (the guard answers 401 first).
    const anon = await fetch(`${h.baseUrl}/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ agent: 'assistant' }),
    });
    assert.equal(anon.status, 401, '/v1/sessions requires a credential');
    console.log('  ok: unauthenticated session call -> 401');

    // A full agent turn on the bare surface (tenancy is key-resolved; there is
    // no project addressing).
    const bare = await converse(client(h.baseUrl, h.keys.api));
    assert.ok(bare.replies.some((t) => t.includes(HELLO)), `bare: ${JSON.stringify(bare.replies)}`);
    console.log('  ok: full agent turn on the bare surface');

    // The x-api-key credential path authenticates too.
    const viaApiKey = await converse(client(h.baseUrl, h.keys.api, { header: 'x-api-key' }));
    assert.ok(viaApiKey.replies.some((t) => t.includes(HELLO)), 'x-api-key path authenticates');
    console.log('  ok: x-api-key credential path also authenticates');

    // SSE stream (events.stream) carries the turn's events.
    const sdk = client(h.baseUrl, h.keys.api);
    const streamed = await converse(sdk);
    const stream = await sdk.beta.sessions.events.stream(streamed.id, { betas: BETAS });
    const streamedTypes = [];
    for await (const ev of stream) streamedTypes.push(ev.type);
    assert.ok(streamedTypes.includes('agent.message'), `stream types: ${streamedTypes}`);
    console.log('  ok: SSE stream (events.stream) carries the agent turn');
  } finally {
    await h.stop();
  }
}

// ── Phase 2: durability — a session survives a process restart ────────────────
async function durabilityAcrossRestart(bin) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-standalone-'));
  const env = { AWAKEN_STORAGE_DIR: dir };

  // Boot 1: create a session AND run one turn — the config lands in
  // <dir>/sessions.db and the transcript commits to the durable store under <dir>.
  const first = startStandalone(bin, BASE_PORT + 1, env);
  let sessionId;
  try {
    await ready(first, BASE_PORT + 1);
    const turn = await converse(client(first.baseUrl, first.keys.api));
    assert.ok(turn.replies.some((t) => t.includes(HELLO)), 'the pre-restart turn ran');
    sessionId = turn.id;
  } finally {
    await first.stop();
  }

  // Boot 2 over the SAME dir: a fresh process (fresh keys) continues the SAME
  // session — the server rehydrates its config (from sessions.db) and transcript
  // (from the commit store), and the agent answers again. That the session is
  // reachable at all after a full process death is the durability guarantee.
  const second = startStandalone(bin, BASE_PORT + 2, env);
  try {
    await ready(second, BASE_PORT + 2);
    const sdk = client(second.baseUrl, second.keys.api);
    await sdk.beta.sessions.events.send(sessionId, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'again' }] }],
      betas: BETAS,
    });
    const events = [];
    for await (const ev of sdk.beta.sessions.events.list(sessionId, { betas: BETAS })) {
      events.push(ev);
    }
    const replies = events.filter((e) => e.type === 'agent.message').length;
    assert.ok(replies >= 2, `the session rehydrated and continued after restart (${replies} replies)`);
    console.log('  ok: a session survives a full process restart (durable store + session repo)');
  } finally {
    await second.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

async function main() {
  const bin = standaloneBin();
  try {
    await enforcementAndAgentLoop(bin);
    await durabilityAcrossRestart(bin);
    console.log('E2E PASS: awaken-standalone full protocol surface + enforcement + durability (deployment-agnostic).');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
