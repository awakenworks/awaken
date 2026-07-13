// End-to-end for the aggregated `awaken` command (crate awaken-cli), Serve role.
//
// `awaken` is the single binary that subsumes awaken-server-local and
// awaken-standalone: configuration (AWAKEN_ROLE + the deployment axes) decides the
// deployment. The default Serve role mounts the production management assembly, whose
// host resolves each session's model from the **database-configured** catalog +
// credential vault (ConfigExecutorProvider) — not a baked-in demo model.
//
// This test proves that path through the real binary: author a provider / endpoint /
// offering + an Anthropic credential through the console API, publish an agent bound
// to that model, then run a session and assert the reply came from the configured
// model over the wire (a fake Anthropic upstream). It is the first coverage of the
// console-config → resolve → run chain end to end.
//
// Run: (from e2e/)  npm install && node awaken_cli_e2e.mjs

import assert from 'node:assert/strict';
import net from 'node:net';
import readline from 'node:readline';
import { spawn, execSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38411);
const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-awaken-cli-fake-key'; // awaken-allow: secret
// The management plane's per-provider credential derive resolves in this workspace
// (authz::BOOTSTRAP_WORKSPACE); the console credential must land here to be picked up.
const WORKSPACE = 'wrkspc_default';
const AGENT = 'db-model-agent';
const MODEL = 'fake-haiku';
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
  readline.createInterface({ input: server.stderr }).on('line', (line) => {
    process.stderr.write(`${line}\n`);
  });
  const stop = () =>
    new Promise((resolve) => {
      if (server.exitCode !== null) return resolve();
      server.on('exit', () => resolve());
      server.kill('SIGINT');
    });
  return { server, baseUrl: `http://127.0.0.1:${port}`, stop };
}

async function req(base, method, uri, body) {
  const res = await fetch(`${base}${uri}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    json = { _raw: text };
  }
  return { status: res.status, json };
}

async function ready(base, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      const res = await fetch(`${base}/v1/capabilities`);
      if (res.ok) return;
    } catch {
      /* not up yet */
    }
    if (Date.now() > deadline) throw new Error('management plane did not become ready');
    await sleep(200);
  }
}

async function main() {
  const upstream = await startFakeAnthropic(FAKE_KEY);
  const bin = awakenBin();
  // In-memory management stores (no AWAKEN_MGMT_DIR): the console config lives for the
  // process lifetime, which is all this resolve→run proof needs.
  const h = startAwaken(bin, PORT);
  try {
    await waitForPort(PORT);
    await ready(h.baseUrl);
    console.log('ok: aggregated `awaken` command booted in the default Serve role (management plane)');

    // ---- author the model in the database-backed console ---------------------
    const base = h.baseUrl;
    let r = await req(base, 'PUT', '/v1/config/providers/anthropic', {
      id: 'anthropic', slug: 'anthropic', display_name: 'Anthropic', version: 1,
    });
    assert.equal(r.status, 200, `provider: ${JSON.stringify(r.json)}`);
    r = await req(base, 'PUT', '/v1/config/endpoints/ep1', {
      id: 'ep1', provider_id: 'anthropic', dialect: 'anthropic_messages',
      base_url: `${upstream.url}/v1/`, timeout_secs: 300, display_name: 'fake', version: 1,
    });
    assert.equal(r.status, 200, `endpoint: ${JSON.stringify(r.json)}`);
    r = await req(base, 'POST', '/v1/config/offerings', {
      model_id: MODEL, provider_id: 'anthropic',
      protocol_endpoint_id: 'ep1', dialect: 'anthropic_messages', upstream_model: null,
    });
    assert.equal(r.status, 200, `offering: ${JSON.stringify(r.json)}`);
    r = await req(base, 'POST', '/v1/config/credentials', {
      workspace_id: WORKSPACE, kind: 'vault', provider_id: 'anthropic',
      env_key: 'ANTHROPIC_API_KEY', secret: FAKE_KEY,
    });
    assert.equal(r.status, 201, `credential: ${JSON.stringify(r.json)}`);
    console.log('ok: authored provider/endpoint/offering + credential in the console DB');

    // ---- publish an agent bound to that model --------------------------------
    // The console agent object is the managed `/v1/agents` shape: the model is
    // `model: { id }`, which the config plane maps to a Pinned selection — so a
    // session for this agent runs exactly the DB-configured `MODEL`.
    r = await req(base, 'PUT', `/v1/config/agents/${AGENT}`, {
      name: AGENT,
      model: { id: MODEL },
      system: 'You are a test agent.',
      max_steps: 2,
    });
    assert.equal(r.status, 200, `agent config: ${JSON.stringify(r.json)}`);
    r = await req(base, 'POST', `/v1/config/agents/${AGENT}/publish`, undefined);
    assert.equal(r.status, 200, `publish: ${JSON.stringify(r.json)}`);
    assert.equal(r.json.installed, true, 'published agent installed into the live catalog');
    console.log(`ok: published agent bound to the DB-configured model '${MODEL}'`);

    // ---- run a session on the DB-configured model ----------------------------
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const session = await client.beta.sessions.create({
      agent: AGENT, environment_id: 'env_local', betas: BETAS,
    });
    assert.ok(session.id.startsWith('sesn_'), `session id: ${session.id}`);
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'resolve me' }] }],
      betas: BETAS,
    });
    const events = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
    const msg = events.find((e) => e.type === 'agent.message');
    assert.ok(msg, `expected an agent.message in ${events.map((e) => e.type)}`);
    const text = (msg.content ?? []).map((c) => c.text ?? '').join('');
    assert.ok(
      text.includes('FAKE:resolve me'),
      `the session ran the DB-configured model over the wire: ${JSON.stringify(text)}`,
    );
    assert.ok(upstream.requests.length >= 1, 'the fake upstream received the configured-model call');
    console.log('ok: session ran the database-configured model + credential over the wire');
  } finally {
    await h.stop();
    upstream.close();
  }
  console.log('\nawaken_cli_e2e: PASS');
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
