// Deployment-agnostic e2e for the OPEN single-machine binary (awaken-standalone).
//
// It is black-box over real HTTP with the official Anthropic TypeScript SDK, so
// the exact same assertions hold against any deployment form that speaks the
// Managed Agents API (standalone / private / cloud) — enforcement and the agent
// loop are properties of the shared open runtime, not of a deployment. What is
// specific to the standalone: it seeds a singleton tenant + two keys at boot
// (printed once to stderr) and serves the session surface behind the session-axis
// guard, so every call needs a bearer credential.
//
// Run: (from e2e/)  npm install && node standalone_e2e.mjs

import assert from 'node:assert/strict';
import net from 'node:net';
import readline from 'node:readline';
import { spawn, execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38311);
const ADDR = `127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const HELLO = 'Hello from awaken-standalone.';

// Build the standalone binary once and resolve its path (spawn the binary, not
// `cargo run`, so the harness can kill a single process cleanly).
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

// Drive one full turn (create → user.message → list) and return the agent texts.
async function converse(client) {
  const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
  assert.ok(session.id.startsWith('sesn_'), `session id: ${session.id}`);
  await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'hi' }] }],
    betas: BETAS,
  });
  const events = [];
  for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
    events.push(ev);
  }
  return events.filter((e) => e.type === 'agent.message').map((e) => e.content[0].text);
}

async function main() {
  const bin = standaloneBin();
  const server = spawn(bin, {
    env: { ...process.env, AWAKEN_STANDALONE_ADDR: ADDR },
    stdio: ['ignore', 'inherit', 'pipe'],
  });

  // The standalone prints its seeded keys once, to stderr; capture them (this is
  // the single-machine operator hand-off — there is no HTTP provisioning plane).
  const keys = {};
  readline.createInterface({ input: server.stderr }).on('line', (line) => {
    process.stderr.write(`${line}\n`);
    const admin = line.match(/admin key:\s+(sk-ant-\S+)/);
    const api = line.match(/api key:\s+(sk-ant-\S+)/);
    if (admin) keys.admin = admin[1];
    if (api) keys.api = api[1];
  });
  server.on('exit', (code) => {
    if (code !== null && code !== 0) {
      console.error(`server exited early: ${code}`);
      process.exit(1);
    }
  });

  try {
    await waitForPort(PORT);
    for (let i = 0; i < 100 && !keys.api; i++) await new Promise((r) => setTimeout(r, 50));
    assert.ok(keys.api?.startsWith('sk-ant-'), 'captured the seeded api key from the banner');
    assert.ok(keys.admin?.startsWith('sk-ant-'), 'captured the seeded admin key');
    assert.notEqual(keys.api, keys.admin, 'the two keys are distinct');

    // 1. Every session call needs a credential (the guard answers 401 first).
    const anon = await fetch(`http://${ADDR}/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ agent: 'assistant' }),
    });
    assert.equal(anon.status, 401, 'the bare session surface requires a credential');
    const anonProject = await fetch(`http://${ADDR}/projects/local/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ agent: 'assistant' }),
    });
    assert.equal(anonProject.status, 401, 'the project session surface requires a credential');
    console.log('  ok: unauthenticated session calls -> 401 on both axes');

    // 2. A full agent turn on the bare surface with the seeded api key.
    const bare = new Anthropic({ apiKey: null, authToken: keys.api, baseURL: `http://${ADDR}` });
    const bareReplies = await converse(bare);
    assert.ok(
      bareReplies.some((t) => t.includes(HELLO)),
      `bare agent replied: ${JSON.stringify(bareReplies)}`,
    );
    console.log('  ok: authenticated full agent turn (bare surface)');

    // 3. The same turn under the project prefix (addressing changes, wire does not).
    const project = new Anthropic({
      apiKey: null,
      authToken: keys.api,
      baseURL: `http://${ADDR}/projects/local`,
    });
    const projectReplies = await converse(project);
    assert.ok(
      projectReplies.some((t) => t.includes(HELLO)),
      `project agent replied: ${JSON.stringify(projectReplies)}`,
    );
    console.log('  ok: authenticated full agent turn (/projects/local prefix)');

    // 3b. The SDK's api-key path (x-api-key header, not Bearer) authenticates too.
    const viaApiKey = new Anthropic({ apiKey: keys.api, baseURL: `http://${ADDR}` });
    const apiKeyReplies = await converse(viaApiKey);
    assert.ok(
      apiKeyReplies.some((t) => t.includes(HELLO)),
      `x-api-key agent replied: ${JSON.stringify(apiKeyReplies)}`,
    );
    console.log('  ok: x-api-key credential path also authenticates');

    // 4. An unauthored project is 404 (addressing resolves before the session).
    const ghost = await fetch(`http://${ADDR}/projects/ghost/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: `Bearer ${keys.api}` },
      body: JSON.stringify({ agent: 'assistant' }),
    });
    assert.equal(ghost.status, 404, 'an unauthored project is not found');
    console.log('  ok: unknown project -> 404');

    console.log('E2E PASS: awaken-standalone enforced session surface + agent turn (deployment-agnostic).');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    server.kill('SIGINT');
  }
}

main();
