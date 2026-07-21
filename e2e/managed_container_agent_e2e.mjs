// Full external SDK -> managed -> CONTAINER agent, against a REAL Docker daemon.
//
// The deepest sandbox seam: an ACP agent running as a **process-as-container** in a
// real Docker container, driven end-to-end through the managed protocol. The brain
// (scenario-host, `AWAKEN_MODEL_MODE=acp-container`, built `--features container-docker`)
// creates one Session-owned environment, starts the production hand in it, and execs
// a deterministic newline ACP fixture in that SAME environment. Seeing the fixture's
// marker proves: external SDK -> managed session -> environment create -> bound ACP
// exec -> response; inspecting the container proves no per-attempt environment exists.
//
// The k8s POD mechanics of the same seam are covered by the k8s adapter e2e
// (`awaken-sandbox-container/tests/k8s_e2e.rs`) + the k3d topology e2e; Docker keeps
// this managed-protocol proof to a single daemon (no cluster).
//
// Self-skips when Docker is unreachable. Run: (from e2e/) node managed_container_agent_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import { spawn, execFileSync, execSync, spawnSync } from 'node:child_process';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { REPO_ROOT } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38143);
const BETAS = ['managed-agents-2026-04-01'];
const MARKER = 'CONTAINER-AGENT-OK';
const IMAGE = process.env.AWAKEN_TEST_SESSION_IMAGE ?? 'awaken-sandbox:session-e2e';
const TMP = `/tmp/awaken-container-agent-e2e-${process.pid}`;
const ACP_FIXTURE = `process.stdin.once('data',()=>{console.log(JSON.stringify({type:'message',text:'${MARKER}'}));console.log(JSON.stringify({type:'turn_end',reason:'natural_end'}))})`;

function git(args, cwd) {
  return execFileSync('git', args, { cwd, encoding: 'utf8' });
}

function seedSkillRepository() {
  const work = `${TMP}/skill-seed`;
  fs.mkdirSync(`${work}/greet`, { recursive: true });
  git(['init', '-q', '-b', 'main'], work);
  git(['config', 'user.email', 'container-e2e@awaken.invalid'], work);
  git(['config', 'user.name', 'Awaken Container E2E'], work);
  fs.writeFileSync(
    `${work}/greet/SKILL.md`,
    '---\nname: greet\ndescription: container skill\n---\nCONTAINER-SKILL-OK\n',
  );
  git(['add', '-A'], work);
  git(['commit', '-q', '-m', 'seed container skill'], work);
  const bare = `${TMP}/skills.git`;
  git(['clone', '-q', '--bare', work, bare]);
  return bare;
}

function dockerAvailable() {
  return spawnSync('docker', ['version'], { stdio: 'ignore' }).status === 0;
}

function testContainers({ all = false } = {}) {
  const args = ['ps'];
  if (all) args.push('-a');
  args.push('-q', '--filter', 'label=awaken.sandbox=1', '--filter', `ancestor=${IMAGE}`);
  return execFileSync('docker', args, { encoding: 'utf8' }).trim().split(/\s+/).filter(Boolean);
}

function cleanupTestContainers() {
  const containers = testContainers({ all: true });
  if (containers.length > 0) spawnSync('docker', ['rm', '-f', ...containers], { stdio: 'ignore' });
}

// Build the canonical production image, but omit network-fetched ACP packages: this
// hermetic scenario supplies a tiny Node newline fixture through AWAKEN_ACP_ARGV.
// The image still contains the real `awaken-sandbox hand --stdio` binary.
function ensureSessionImage() {
  if (spawnSync('docker', ['image', 'inspect', IMAGE], { stdio: 'ignore' }).status === 0) return;
  execFileSync('deploy/images/sandbox/build.sh', [IMAGE, ''], {
    cwd: REPO_ROOT,
    env: process.env,
    stdio: 'inherit',
  });
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
  ensureSessionImage();
  cleanupTestContainers();
  fs.rmSync(TMP, { recursive: true, force: true });
  fs.mkdirSync(TMP, { recursive: true });
  const skillRepository = seedSkillRepository();
  const bin = buildBrain();
  const addr = `127.0.0.1:${PORT}`;
  const brain = spawn(bin, {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: addr,
      AWAKEN_MODEL_MODE: 'acp-container',
      AWAKEN_CONTAINER_IMAGE: IMAGE,
      AWAKEN_SANDBOX_TIER: 'docker',
      AWAKEN_ACP_ARGV: `node -e ${ACP_FIXTURE}`,
      // Disable the reaper's periodic sweep noise during the short test; the startup
      // sweep still runs (proving it is harmless with no leaked containers present).
      AWAKEN_SANDBOX_REAP_INTERVAL: '3600',
    },
    stdio: ['ignore', 'inherit', 'inherit'],
  });

  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${addr}` });
    const file = await client.beta.files.upload({
      file: await toFile(Buffer.from('CONTAINER-FILE-OK'), 'input.txt'),
      betas: BETAS,
    });
    const memory = await client.post('/v1/memory_stores');
    await client.post(`/v1/memory_stores/${memory.id}/memories`, {
      body: { path: '/seed.txt', content: 'CONTAINER-MEMORY-SEED' },
    });
    await client.post('/v1/skills', {
      body: {
        id: 'delivered-container',
        content: '---\ndescription: delivered container skill\n---\nCONTAINER-DELIVERED-SKILL-OK',
      },
    });

    const session = await client.beta.sessions.create({
      agent: 'assistant',
      metadata: { 'awaken.runtime': 'acp:custom' },
      environment_id: 'env_local',
      resources: [
        { type: 'file', file_id: file.id, mount_path: '/workspace/input.txt' },
        { type: 'memory_store', memory_store_id: memory.id, mount_path: '/notes' },
        { type: 'github_repository', url: skillRepository, mount_path: '/workspace/skills' },
      ],
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

    const containers = testContainers();
    assert.equal(containers.length, 1, 'the Session must own one shared container, not one per attempt');
    const container = containers[0];
    assert.equal(
      execFileSync('docker', ['exec', container, 'cat', '/workspace/.mnt/workspace/input.txt'], {
        encoding: 'utf8',
      }),
      'CONTAINER-FILE-OK',
      'the uploaded file must be materialized into the Session container',
    );
    assert.match(
      execFileSync('docker', ['exec', container, 'cat', '/workspace/skills/greet/SKILL.md'], {
        encoding: 'utf8',
      }),
      /CONTAINER-SKILL-OK/,
      'the repository-backed workspace skill must be imported into the Session container',
    );
    const deliveredSkill = '/workspace/.skills/delivered-container/SKILL.md';
    assert.match(
      execFileSync('docker', ['exec', container, 'cat', deliveredSkill], { encoding: 'utf8' }),
      /CONTAINER-DELIVERED-SKILL-OK/,
      'the durable delivered-skill bundle must be materialized into the Session container',
    );
    execFileSync('docker', [
      'exec',
      container,
      'sh',
      '-c',
      'test ! -w "$1"',
      'awaken-skill-check',
      deliveredSkill,
    ]);
    assert.equal(
      execFileSync('docker', ['exec', container, 'cat', '/workspace/.mnt/notes/seed.txt'], {
        encoding: 'utf8',
      }),
      'CONTAINER-MEMORY-SEED',
      'the governed memory filesystem must hydrate into the Session container',
    );

    execFileSync('docker', [
      'exec',
      container,
      'sh',
      '-c',
      'printf %s CONTAINER-MEMORY-OK > /workspace/.mnt/notes/container.txt',
    ]);
    await client.get(`/v1/files?scope_id=${session.id}`);
    const harvested = await client.get(`/v1/memory_stores/${memory.id}/memories`);
    assert.equal(
      harvested.data.find((entry) => entry.path === '/container.txt')?.content,
      'CONTAINER-MEMORY-OK',
      'container memory writes must harvest through the host',
    );

    console.log(
      'E2E PASS: container agent — ACP, hand, file, memory, repository, workspace skill and immutable delivered skill shared one Session-owned Docker environment.',
    );
  } finally {
    brain.kill('SIGINT');
    cleanupTestContainers();
    fs.rmSync(`/tmp/awaken-acp-container-${brain.pid}`, { recursive: true, force: true });
    fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
