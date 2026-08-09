// Real Codex ACP end-to-end release gate.
//
// One script owns both deployment profiles:
//
//   CODEX_ACP_LIVE=1 CODEX_ACP_PROFILE=host-login \
//     node e2e/codex_acp_live_e2e.mjs
//
//   CODEX_ACP_LIVE=1 CODEX_ACP_PROFILE=container \
//     CODEX_ACP_BASE_URL=http://127.0.0.1:38080 \
//     node e2e/codex_acp_live_e2e.mjs
//
// Cause/effect graph:
// C1 explicit live opt-in; C2 deployment profile is host-login or container;
// C3 Codex owns an available login; C4 Awaken publishes a BackendDefault Agent;
// C5 one real prompt completes. E1 the committed transcript contains the unique
// marker and running -> idle states. Host-login additionally proves zero-config
// discovery plus a live Worker capability; container additionally proves a new
// managed sandbox was observed.
//
// Decision table:
// | Rule | C1 | C2 | C3-C5 | Effect |
// | H1 | Y | host-login | Y | E1 + live local capability, no auth-file access |
// | C1 | Y | container  | Y | E1 + newly observed managed container |
// | F1 | N | any        | - | fail before invoking Codex |
// | F2 | Y | other      | - | fail before server/session work |

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { closeHttpServer } from './http_server.mjs';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execSync, spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { waitForVerifiedAcpCapability } from './fixtures/acp_capability.mjs';

if (process.env.CODEX_ACP_LIVE !== '1') {
  throw new Error('set CODEX_ACP_LIVE=1 to confirm this test may invoke the real Codex ACP adapter');
}

const profile = process.env.CODEX_ACP_PROFILE;
if (!['host-login', 'container'].includes(profile)) {
  throw new Error('set CODEX_ACP_PROFILE=host-login or CODEX_ACP_PROFILE=container');
}

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const BETAS = ['managed-agents-2026-04-01'];
const marker = `CODEX-ACP-READY-${Date.now()}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function managedContainerIds() {
  const result = spawnSync('docker', ['ps', '-q', '--filter', 'label=awaken.sandbox=1'], {
    encoding: 'utf8',
  });
  assert.equal(
    result.status,
    0,
    `Docker is required for this gate: ${result.stderr || result.error || ''}`,
  );
  return new Set(result.stdout.trim().split(/\s+/).filter(Boolean));
}

function awakenBin() {
  const output = execSync(
    'cargo build --quiet --message-format=json -p awaken-cli --bin awaken',
    { cwd: ROOT, maxBuffer: 128 * 1024 * 1024 },
  ).toString();
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken') return message.executable;
    } catch {
      // Cargo diagnostic.
    }
  }
  throw new Error('could not resolve the awaken binary');
}

async function availablePort() {
  const server = net.createServer();
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  assert.equal(typeof address, 'object');
  await closeHttpServer(server);
  return address.port;
}

async function waitReady(child, baseURL) {
  const deadline = Date.now() + 180_000;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`${baseURL}/v1/capabilities`);
      if (response.ok) return;
    } catch {
      // Startup is still in progress.
    }
    if (child.exitCode !== null) {
      throw new Error(`awaken exited before readiness with ${child.exitCode}`);
    }
    await sleep(200);
  }
  throw new Error('awaken did not become ready');
}

async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise((resolve) => child.once('exit', resolve));
  child.kill('SIGINT');
  if (await Promise.race([exited.then(() => true), sleep(10_000).then(() => false)])) return;
  child.kill('SIGKILL');
  await exited;
}

async function request(baseURL, method, route, body) {
  const response = await fetch(`${baseURL}${route}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = await response.json().catch(() => ({}));
  return { response, value };
}

async function startHostLoginProfile() {
  const binary = awakenBin();
  const doctor = spawnSync(binary, ['doctor', 'acp', '--json'], {
    env: process.env,
    encoding: 'utf8',
  });
  assert.equal(doctor.status, 0, doctor.stderr);
  const codex = JSON.parse(doctor.stdout).acp.find((row) => row.id === 'codex');
  assert.equal(codex?.login_state, 'available', JSON.stringify(codex));

  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-codex-host-live-'));
  const port = await availablePort();
  const config = path.join(temp, 'config.toml');
  fs.writeFileSync(config, [
    `data_dir = ${JSON.stringify(path.join(temp, 'data'))}`,
    `bind = ${JSON.stringify(`127.0.0.1:${port}`)}`,
    `control_seal_key = ${JSON.stringify(randomBytes(32).toString('hex'))}`,
    // Intentionally no sandbox_tier, acp_clis, default backend, or credential
    // setting: host ACP discovery is the zero-configuration product path.
  ].join('\n'));
  const child = spawn(binary, ['all-in-one', '--config', config], {
    env: process.env,
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  const baseURL = `http://127.0.0.1:${port}`;
  try {
    await waitReady(child, baseURL);
    await waitForVerifiedAcpCapability(baseURL, 'codex', {
      timeoutMs: 60_000,
      pollMs: 200,
      requireAvailableLogin: true,
    });

    const agent = process.env.AWAKEN_ACP_AGENT ?? 'codex-host-login-live';
    let result = await request(baseURL, 'PUT', `/v1/config/agents/${agent}`, {
      name: 'Real host Codex login',
      system: 'Return exactly the text requested by the user.',
      model: { mode: 'backend_default', backend_ref: 'acp:codex' },
      tools: [],
    });
    assert.equal(result.response.status, 200, JSON.stringify(result.value));
    result = await request(baseURL, 'POST', `/v1/config/agents/${agent}/publish`);
    assert.equal(result.response.status, 200, JSON.stringify(result.value));
    return { baseURL, agent, cleanup: async () => {
      await stop(child);
      fs.rmSync(temp, { recursive: true, force: true });
    } };
  } catch (error) {
    await stop(child);
    fs.rmSync(temp, { recursive: true, force: true });
    throw error;
  }
}

const deployment = profile === 'host-login'
  ? await startHostLoginProfile()
  : {
      baseURL: process.env.CODEX_ACP_BASE_URL ?? 'http://127.0.0.1:38080',
      agent: process.env.AWAKEN_ACP_AGENT ?? 'codex',
      cleanup: async () => {},
    };

const client = new Anthropic({
  apiKey: 'local-live-gate', // awaken-allow: secret (dummy; local server ignores it)
  baseURL: deployment.baseURL,
});

async function events(sessionId) {
  const out = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    out.push(event);
  }
  return out;
}

const session = await client.beta.sessions.create({
  agent: deployment.agent,
  environment_id: 'env_local',
  betas: BETAS,
});

const baselineContainers = profile === 'container' ? managedContainerIds() : new Set();
const observedContainers = new Set();
const containerProbe = profile === 'container'
  ? setInterval(() => {
      for (const id of managedContainerIds()) {
        if (!baselineContainers.has(id)) observedContainers.add(id);
      }
    }, 100)
  : undefined;

try {
  try {
    await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `Reply with exactly this text and nothing else: ${marker}` }],
      }],
      betas: BETAS,
    });
  } finally {
    if (containerProbe !== undefined) clearInterval(containerProbe);
  }

  if (profile === 'container') {
    assert.ok(
      observedContainers.size > 0,
      'the Codex reply completed without observing a newly-created awaken.sandbox container',
    );
  }

  const transcript = await events(session.id);
  const replies = transcript
    .filter((event) => event.type === 'agent.message')
    .map((event) => (event.content ?? []).map((content) => content.text ?? '').join('').trim());

  assert.ok(
    replies.some((reply) => reply.includes(marker)),
    `real Codex ACP reply did not contain the marker; replies=${JSON.stringify(replies)}`,
  );
  assert.ok(
    transcript.some((event) => event.type === 'session.status_running'),
    `the managed transcript did not expose a running state; events=${transcript.map((event) => event.type)}`,
  );
  assert.ok(
    transcript.some((event) => event.type === 'session.status_idle'),
    `the managed transcript did not return to idle; events=${transcript.map((event) => event.type)}`,
  );

  console.log(
    `CODEX ACP ${profile.toUpperCase()} E2E PASS: ${session.id} committed ${marker}`,
  );
} finally {
  await deployment.cleanup();
}
