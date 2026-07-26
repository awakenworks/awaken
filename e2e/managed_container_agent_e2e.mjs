// Full external SDK -> managed -> CONTAINER agent, against a real Docker or Podman daemon.
//
// The deepest sandbox seam: an ACP agent running as a **process-as-container** in a
// real container, driven end-to-end through the managed protocol. The brain
// (scenario-host, `AWAKEN_MODEL_MODE=acp-container`, built with the selected backend)
// creates one Session-owned environment, starts the production hand in it, and execs
// a deterministic newline ACP fixture in that SAME environment. Seeing the fixture's
// marker proves: external SDK -> managed session -> environment create -> bound ACP
// exec -> response; inspecting the container proves no per-attempt environment exists.
//
// The k8s POD mechanics of the same seam are covered by the k8s adapter e2e
// (`awaken-sandbox-container/tests/k8s_e2e.rs`) + the k3d topology e2e; Docker and
// Podman keep this managed-protocol proof to a single daemon (no cluster).
//
// Select Podman with `AWAKEN_E2E_CONTAINER_ENGINE=podman`. The standalone npm suite
// self-skips when the selected engine is unavailable; the stage gate sets
// `AWAKEN_E2E_REQUIRE_CONTAINER=1` and fails closed instead.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import { spawn, execFileSync, execSync, spawnSync } from 'node:child_process';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { REPO_ROOT } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38143);
const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const MARKER = 'CONTAINER-AGENT-OK';
const ENGINE = process.env.AWAKEN_E2E_CONTAINER_ENGINE ?? 'docker';
assert.ok(['docker', 'podman'].includes(ENGINE), `unsupported container engine ${ENGINE}`);
const IMAGE = process.env.AWAKEN_TEST_SESSION_IMAGE ?? 'awaken-sandbox:session-e2e';
const TMP = `/tmp/awaken-container-agent-${ENGINE}-e2e-${process.pid}`;
const ACP_FIXTURE = `process.stdin.once('data',()=>{console.log(JSON.stringify({type:'message',text:'${MARKER}'}));console.log(JSON.stringify({type:'turn_end',reason:'natural_end'}))})`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function afterPendingActivation(operation) {
  let last;
  for (let attempt = 0; attempt < 50; attempt += 1) {
    try {
      return await operation();
    } catch (error) {
      last = error;
      if (error.status !== 400 || !String(error.message).includes('activation is already pending')) throw error;
      await sleep(100);
    }
  }
  throw last;
}

async function exercisePodmanEnvironment(client, name, sandbox, expectSuccess) {
  const network = sandbox.network;
  const environment = await client.beta.environments.create({
    name: `podman-${name}`,
    config: {
      type: 'cloud',
      networking: !network || network.mode === 'unrestricted'
        ? { type: 'unrestricted' }
        : { type: 'limited', allowed_hosts: network.hosts ?? [] },
    },
    betas: BETAS,
  });
  const policyId = `podman-${name}-${process.pid}`;
  const { network: _ownedByEnvironment, ...policyConfig } = sandbox;
  let response = await fetch(`http://127.0.0.1:${PORT}/v1/awaken/sandbox-execution-policies`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ id: policyId, config: policyConfig }),
  });
  assert.equal(response.status, 201, await response.text());
  response = await fetch(
    `http://127.0.0.1:${PORT}/v1/awaken/environments/${environment.id}/sandbox-execution-policy`,
    {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ policy_id: policyId, version: 1 }),
    },
  );
  assert.equal(response.status, 200, await response.text());
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    metadata: { 'awaken.runtime': 'acp:custom' },
    environment_id: environment.id,
    betas: BETAS,
  });
  let sendFailure;
  try {
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: `exercise ${name}` }] }],
      betas: BETAS,
    });
  } catch (error) {
    sendFailure = error;
  }
  const events = [];
  for await (const event of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
    events.push(event);
  }
  if (expectSuccess) {
    assert.equal(sendFailure, undefined, `${name} should realize: ${sendFailure}`);
    assert.ok(
      events.some(
        (event) => event.type === 'agent.message'
          && (event.content ?? []).some((content) => String(content.text ?? '').includes(MARKER)),
      ),
      `${name} must run the containerized agent: ${JSON.stringify(events)}`,
    );
  } else {
    assert.ok(
      sendFailure || events.some((event) => event.type === 'session.error'),
      `${name} must fail closed instead of falling back to the default image: ${JSON.stringify(events)}`,
    );
  }
  await client.beta.sessions.delete(session.id, { betas: BETAS });
  await client.beta.environments.delete(environment.id, { betas: BETAS });
}

async function exercisePodmanRootfsMatrix(client) {
  // Network admission cause/effect graph:
  // C1=allowlist requested; C2=provider proves no-bypass enforcement.
  // C1+!C2 -> N1 fail closed. !C1 -> N2 proceed using the selected rootfs.
  //
  // | Rule | network request | provider proof | result              |
  // | N1   | allowlist       | absent         | reject, no fallback |
  // | N2   | none/default    | n/a            | realize rootfs      |
  await exercisePodmanEnvironment(client, 'host-userland', {
    environment: { kind: 'sandbox' },
    network: { mode: 'allowlist', hosts: ['example.invalid'] },
    limits: { cpu_millis: 1000, memory_bytes: 536870912, pids: 128 },
  }, false);
  await exercisePodmanEnvironment(client, 'explicit-image', {
    environment: { kind: 'image', reference: IMAGE },
    network: { mode: 'none' },
  }, true);
  await exercisePodmanEnvironment(client, 'scope-fallback', {
    environment: { kind: 'scope' },
  }, true);
  await exercisePodmanEnvironment(client, 'local-dir-fallback', {
    environment: { kind: 'local_dir', path_template: '/tmp/not-a-container-root' },
  }, true);
  await exercisePodmanEnvironment(client, 'readonly-root', {
    environment: {
      kind: 'isolated_root',
      base: { source: 'dir', path_template: `${TMP}/missing-readonly-root` },
      writable_base: false,
    },
  }, false);
  await exercisePodmanEnvironment(client, 'writable-root', {
    environment: {
      kind: 'isolated_root',
      base: { source: 'dir', path_template: `${TMP}/missing-writable-root` },
      writable_base: true,
    },
  }, false);
  await exercisePodmanEnvironment(client, 'tarball-root', {
    environment: {
      kind: 'isolated_root',
      base: { source: 'tarball', reference: `${TMP}/missing-root.tar` },
      writable_base: false,
    },
  }, false);
  const remaining = testContainerNames({ all: true });
  assert.ok(
    remaining.every((name) => name.includes('-warmpool_')),
    `every rootfs matrix Session must be released; only unused warm capacity may remain: ${remaining}`,
  );
}

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

function containerAvailable() {
  return spawnSync(ENGINE, ['version'], { stdio: 'ignore' }).status === 0;
}

function testContainers({ all = false } = {}) {
  const args = ['ps'];
  if (all) args.push('-a');
  args.push('-q', '--filter', 'label=awaken.sandbox=1', '--filter', `ancestor=${IMAGE}`);
  return execFileSync(ENGINE, args, { encoding: 'utf8' }).trim().split(/\s+/).filter(Boolean);
}

function testContainerNames({ all = false } = {}) {
  const args = ['ps'];
  if (all) args.push('-a');
  args.push('--format', '{{.Names}}', '--filter', 'label=awaken.sandbox=1', '--filter', `ancestor=${IMAGE}`);
  return execFileSync(ENGINE, args, { encoding: 'utf8' }).trim().split(/\s+/).filter(Boolean);
}

function cleanupTestContainers() {
  const containers = testContainers({ all: true });
  if (containers.length > 0) spawnSync(ENGINE, ['rm', '-f', ...containers], { stdio: 'ignore' });
}

// Build the canonical production image, but omit network-fetched ACP packages: this
// hermetic dev scenario supplies a tiny Node newline fixture through its explicit
// fixed launch input (read from AWAKEN_ACP_ARGV only by the scenario host).
// The image still contains the real `awaken-sandbox hand --stdio` binary.
function ensureSessionImage() {
  if (spawnSync(ENGINE, ['image', 'inspect', IMAGE], { stdio: 'ignore' }).status === 0) return;
  execFileSync('deploy/images/sandbox/build.sh', [IMAGE, ''], {
    cwd: REPO_ROOT,
    env: { ...process.env, CONTAINER_ENGINE: ENGINE },
    stdio: 'inherit',
  });
}

// Build the brain with the selected container feature (the shared harness builds
// default features only), and resolve the binary path from cargo's JSON output.
function buildBrain() {
  const out = execSync(
    `cargo build --quiet --message-format=json -p awaken-scenario-host --bin awaken-scenario-host --features container-${ENGINE}`,
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
  if (!containerAvailable()) {
    if (process.env.AWAKEN_E2E_REQUIRE_CONTAINER === '1') {
      throw new Error(`required ${ENGINE} runtime is unavailable`);
    }
    console.log(`E2E SKIP: no reachable ${ENGINE} runtime.`);
    return;
  }
  ensureSessionImage();
  cleanupTestContainers();
  fs.rmSync(TMP, { recursive: true, force: true });
  fs.mkdirSync(TMP, { recursive: true });
  const skillRepository = seedSkillRepository();
  const bin = buildBrain();
  const addr = `127.0.0.1:${PORT}`;
  const brainEnv = {
    ...process.env,
    AWAKEN_HTTP_ADDR: addr,
    AWAKEN_MODEL_MODE: 'acp-container',
    AWAKEN_CONTAINER_IMAGE: IMAGE,
    SESSION_ENVIRONMENT_TIER: ENGINE,
    SESSION_DEPLOYMENT_STORAGE_DIR: `${TMP}/storage`,
    AWAKEN_ACP_ARGV: `node -e ${ACP_FIXTURE}`,
    // Exercise the production pool wrapper. Resource-bearing environments are
    // deliberately non-poolable, so this changes composition without creating a
    // second Session container or weakening the one-environment assertion below.
    AWAKEN_SANDBOX_WARM_POOL: '1',
    // Disable the reaper's periodic sweep noise during the short test; the startup
    // sweep still runs (proving it is harmless with no leaked containers present).
    AWAKEN_SANDBOX_REAP_INTERVAL: '3600',
    AWAKEN_CONTAINER_FORWARD_PROXY: 'http://127.0.0.1:9',
  };
  const spawnBrain = () => spawn(bin, {
    env: brainEnv,
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  let brain = spawnBrain();

  try {
    await waitForPort(PORT);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${addr}` });
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
      execFileSync(ENGINE, ['exec', container, 'cat', '/workspace/.mnt/workspace/input.txt'], {
        encoding: 'utf8',
      }),
      'CONTAINER-FILE-OK',
      'the uploaded file must be materialized into the Session container',
    );
    assert.match(
      execFileSync(ENGINE, ['exec', container, 'cat', '/workspace/skills/greet/SKILL.md'], {
        encoding: 'utf8',
      }),
      /CONTAINER-SKILL-OK/,
      'the repository-backed workspace skill must be imported into the Session container',
    );
    const deliveredSkill = '/workspace/.skills/delivered-container/SKILL.md';
    assert.match(
      execFileSync(ENGINE, ['exec', container, 'cat', deliveredSkill], { encoding: 'utf8' }),
      /CONTAINER-DELIVERED-SKILL-OK/,
      'the durable delivered-skill bundle must be materialized into the Session container',
    );
    execFileSync(ENGINE, [
      'exec',
      container,
      'sh',
      '-c',
      'test ! -w "$1"',
      'awaken-skill-check',
      deliveredSkill,
    ]);
    assert.equal(
      execFileSync(ENGINE, ['exec', container, 'cat', '/workspace/.mnt/notes/seed.txt'], {
        encoding: 'utf8',
      }),
      'CONTAINER-MEMORY-SEED',
      'the governed memory filesystem must hydrate into the Session container',
    );

    const liveFile = await client.beta.files.upload({
      file: await toFile(Buffer.from('CONTAINER-LIVE-FILE-OK'), 'live.txt'),
      betas: BETAS,
    });
    const fileResource = await afterPendingActivation(() => client.beta.sessions.resources.add(session.id, {
      type: 'file',
      file_id: liveFile.id,
      mount_path: '/workspace/live.txt',
      betas: BETAS,
    }));
    const repoResource = await afterPendingActivation(() => client.beta.sessions.resources.add(session.id, {
      type: 'github_repository',
      url: skillRepository,
      mount_path: '/workspace/live-repo',
      betas: BETAS,
    }));
    assert.equal(
      execFileSync(ENGINE, ['exec', container, 'cat', '/workspace/.mnt/workspace/live.txt'], {
        encoding: 'utf8',
      }),
      'CONTAINER-LIVE-FILE-OK',
      'a live file attach must update the resident container workspace',
    );
    assert.match(
      execFileSync(ENGINE, ['exec', container, 'cat', '/workspace/live-repo/greet/SKILL.md'], {
        encoding: 'utf8',
      }),
      /CONTAINER-SKILL-OK/,
      'a live repository attach must update the resident container workspace',
    );

    await afterPendingActivation(() => client.beta.sessions.resources.update(fileResource.id, {
      session_id: session.id,
      mount_path: '/workspace/renamed.txt',
      betas: BETAS,
    }));
    await afterPendingActivation(() => client.beta.sessions.resources.update(repoResource.id, {
      session_id: session.id,
      mount_path: '/workspace/renamed-repo',
      betas: BETAS,
    }));
    execFileSync(ENGINE, [
      'exec',
      container,
      'sh',
      '-c',
      'test ! -e /workspace/.mnt/workspace/live.txt && test ! -e /workspace/live-repo',
    ]);
    assert.equal(
      execFileSync(ENGINE, ['exec', container, 'cat', '/workspace/.mnt/workspace/renamed.txt'], {
        encoding: 'utf8',
      }),
      'CONTAINER-LIVE-FILE-OK',
      'a live file rename must revoke the old path and materialize the replacement',
    );
    assert.match(
      execFileSync(ENGINE, ['exec', container, 'cat', '/workspace/renamed-repo/greet/SKILL.md'], {
        encoding: 'utf8',
      }),
      /CONTAINER-SKILL-OK/,
      'a live repository rename must reprovision only the replacement path',
    );

    await afterPendingActivation(() => client.beta.sessions.resources.delete(fileResource.id, {
      session_id: session.id,
      betas: BETAS,
    }));
    await afterPendingActivation(() => client.beta.sessions.resources.delete(repoResource.id, {
      session_id: session.id,
      betas: BETAS,
    }));
    execFileSync(ENGINE, [
      'exec',
      container,
      'sh',
      '-c',
      'test ! -e /workspace/.mnt/workspace/renamed.txt && test ! -e /workspace/renamed-repo',
    ]);

    // A hard brain crash must retain the Session-owned environment. The replacement
    // process restores the durable binding, adopts the exact same container, renews
    // its lease and reconnects both the hand and ACP process before another turn.
    const crashed = new Promise((resolve) => brain.once('exit', resolve));
    brain.kill('SIGKILL');
    await crashed;
    assert.deepEqual(testContainers(), [container], 'a brain crash must not reap the Session container');
    brain = spawnBrain();
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${addr}` });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'resume the adopted container' }] }],
      betas: BETAS,
    });
    const resumedEvents = [];
    for await (const event of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
      resumedEvents.push(event);
    }
    assert.ok(
      resumedEvents.some(
        (event) => event.type === 'agent.message'
          && (event.content ?? []).some((content) => String(content.text ?? '').includes(MARKER)),
      ),
      `the replacement brain must resume through the adopted container: ${JSON.stringify(resumedEvents)}`,
    );
    assert.deepEqual(
      testContainers(),
      [container],
      'replacement must adopt the same container instead of creating an attempt-local duplicate',
    );

    execFileSync(ENGINE, [
      'exec',
      container,
      'sh',
      '-c',
      'printf %s CONTAINER-ARTIFACT-OK > /outputs/result.txt',
    ]);
    const artifacts = await client.get(`/v1/files?scope_id=${session.id}`);
    const artifact = artifacts.data.find((entry) => entry.filename === 'result.txt');
    assert.ok(artifact, `container output must project through the files API: ${JSON.stringify(artifacts)}`);
    const artifactContent = await client.beta.files.download(artifact.id, { betas: BETAS });
    assert.equal(await artifactContent.text(), 'CONTAINER-ARTIFACT-OK');

    execFileSync(ENGINE, [
      'exec',
      container,
      'sh',
      '-c',
      'printf %s CONTAINER-MEMORY-OK > /workspace/.mnt/notes/container.txt',
    ]);
    // Copy-backed MemoryRepository mounts reconcile only at the Session's terminal
    // release edge. Read-only resource APIs must never acquire this write side effect.
    await client.beta.sessions.delete(session.id, { betas: BETAS });
    assert.equal(testContainers({ all: true }).length, 0, 'Session release must reap its container');
    const harvested = await client.get(`/v1/memory_stores/${memory.id}/memories`);
    assert.equal(
      harvested.data.find((entry) => entry.path === '/container.txt')?.content,
      'CONTAINER-MEMORY-OK',
      'container memory writes must reconcile through the host at Session release',
    );

    if (ENGINE === 'podman') {
      await exercisePodmanRootfsMatrix(client);
      console.log('  ok: Managed environment declarations drive Podman image/private-root/network/limit planning');
    }

    console.log(
      `E2E PASS: container agent — ACP, hand, file, memory, repository, workspace skill and immutable delivered skill shared one Session-owned ${ENGINE} environment.`,
    );
  } finally {
    brain.kill('SIGINT');
    if (!process.env.AWAKEN_E2E_KEEP_TMP) cleanupTestContainers();
    fs.rmSync(`/tmp/awaken-acp-container-${brain.pid}`, { recursive: true, force: true });
    if (!process.env.AWAKEN_E2E_KEEP_TMP) fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
