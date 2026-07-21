// Full external SDK -> managed -> CONTAINER agent, against a REAL Docker daemon.
//
// The deepest sandbox seam: an ACP agent running as a **process-as-container** in a
// real Docker container, driven end-to-end through the managed protocol. The brain
// (scenario-host, `AWAKEN_MODEL_MODE=acp-container`, built `--features container-docker`)
// realizes each turn's agent as a container running a deterministic busybox `nc`
// fixture (the same newline-wire double the k8s adapter e2e bakes — no LLM/key needed),
// publishes + dials its port, and round-trips the turn. Seeing the fixture's marker in
// the agent's reply proves: external SDK -> managed session -> container create
// (process-as-container) -> agent wire exchange -> response, all through real Docker.
//
// The k8s POD mechanics of the same seam are covered by the k8s adapter e2e
// (`awaken-sandbox-container/tests/k8s_e2e.rs`) + the k3d topology e2e; Docker keeps
// this managed-protocol proof to a single daemon (no cluster).
//
// Self-skips when Docker is unreachable. Run: (from e2e/) node managed_container_agent_e2e.mjs

import assert from 'node:assert/strict';
import net from 'node:net';
import { spawn, execSync, spawnSync } from 'node:child_process';
import Anthropic from '@anthropic-ai/sdk';
import { REPO_ROOT } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38143);
const BETAS = ['managed-agents-2026-04-01'];
const MARKER = 'CONTAINER-AGENT-OK'; // must match build_acp_container_router's fixture
const IMAGE = 'awaken-bb:1';

function dockerAvailable() {
  return spawnSync('docker', ['version'], { stdio: 'ignore' }).status === 0;
}

// The busybox fixture image the container agent runs in — busybox provides `nc`/`sh`.
// Built by commit (no Dockerfile context needed), mirroring the k8s adapter e2e.
function ensureFixtureImage() {
  if (spawnSync('docker', ['image', 'inspect', IMAGE], { stdio: 'ignore' }).status === 0) return;
  execSync('docker pull -q busybox:1.36', { stdio: 'ignore' });
  spawnSync('docker', ['rm', '-f', 'awaken-bb-tmp'], { stdio: 'ignore' });
  execSync('docker create --name awaken-bb-tmp busybox:1.36 true', { stdio: 'ignore' });
  execSync(`docker commit awaken-bb-tmp ${IMAGE}`, { stdio: 'ignore' });
  spawnSync('docker', ['rm', '-f', 'awaken-bb-tmp'], { stdio: 'ignore' });
}

// Build the brain WITH the container-docker feature (the shared harness builds default
// features only), and resolve the binary path from cargo's JSON output.
function buildBrain() {
  const out = execSync(
    'cargo build --quiet --message-format=json -p awaken-scenario-host --bin awaken-scenario-host --features container-docker',
    { cwd: REPO_ROOT, maxBuffer: 128 * 1024 * 1024 },
  ).toString();
  for (const line of out.split('\n')) {
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.executable && msg.target?.name === 'awaken-scenario-host') return msg.executable;
  }
  throw new Error('could not resolve the scenario-host binary path');
}

function waitForPort(port, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      const s = net.createConnection({ port, host: '127.0.0.1' });
      s.once('connect', () => {
        s.destroy();
        resolve();
      });
      s.once('error', () => {
        s.destroy();
        if (Date.now() > deadline) reject(new Error(`brain did not listen on ${port}`));
        else setTimeout(attempt, 200);
      });
    };
    attempt();
  });
}

async function main() {
  if (!dockerAvailable()) {
    console.log('E2E SKIP: no reachable Docker daemon.');
    return;
  }
  ensureFixtureImage();
  const bin = buildBrain();
  const addr = `127.0.0.1:${PORT}`;
  const brain = spawn(bin, {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: addr,
      AWAKEN_MODEL_MODE: 'acp-container',
      AWAKEN_SANDBOX_IMAGE: IMAGE,
      // Disable the reaper's periodic sweep noise during the short test; the startup
      // sweep still runs (proving it is harmless with no leaked containers present).
      AWAKEN_SANDBOX_REAP_INTERVAL: '3600',
    },
    stdio: ['ignore', 'inherit', 'inherit'],
  });

  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${addr}` });

    // No `awaken.runtime` metadata: the brain is a single-purpose ACP deployment
    // (its default backend IS the containerized ACP agent), so the session routes
    // there from the DEPLOYMENT config, not a client-supplied per-session knob.
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'run the containerized agent' }] }],
      betas: BETAS,
    });

    const events = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);

    const messages = events
      .filter((e) => e.type === 'agent.message')
      .map((e) => (e.content ?? []).map((c) => c.text ?? '').join(''));
    assert.ok(
      messages.some((m) => m.includes(MARKER)),
      `the containerized agent's reply must round-trip to the SDK: ${JSON.stringify(events)}`,
    );

    console.log(
      'E2E PASS: container agent — a managed turn launched a process-as-container agent in real Docker and its newline-wire reply round-tripped to the external SDK.',
    );
  } finally {
    brain.kill('SIGINT');
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
