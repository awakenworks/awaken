// Crash/restart coverage for corrupt or unavailable Session-environment bindings.
//
// Every environment is first realized through the public Managed API and a real
// Docker daemon. The coordinator is then SIGKILLed, the disposable SQLite
// authority is damaged while offline, and a replacement is asked to restore each
// Session through the same public API. No test-only runtime hook is involved.

import assert from 'node:assert/strict';
import { execFileSync, execSync, spawn, spawnSync } from 'node:child_process';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { REPO_ROOT, stopServer, waitForPort } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 39774);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const IMAGE = process.env.AWAKEN_TEST_SESSION_IMAGE ?? 'awaken-sandbox:session-e2e';
const MARKER = 'RECOVERY-BINDING-OK';
const ACP_FIXTURE = `process.stdin.once('data',()=>{console.log(JSON.stringify({type:'message',text:'${MARKER}'}));console.log(JSON.stringify({type:'turn_end',reason:'natural_end'}))})`;

type Binding = {
  provider_kind: string;
  sandbox_id: string;
  extra?: { container_id?: string; outputs_path?: string };
};

function buildBrain(): string {
  const output = execSync(
    'cargo build --quiet --message-format=json -p awaken-scenario-host --bin awaken-scenario-host --features container-docker',
    { cwd: REPO_ROOT, maxBuffer: 128 * 1024 * 1024 },
  ).toString();
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken-scenario-host') return message.executable;
    } catch {
      // Cargo may interleave a non-JSON diagnostic with JSON compiler messages.
    }
  }
  throw new Error('could not resolve the container-enabled scenario host');
}

function ensureImage(): void {
  if (spawnSync('docker', ['image', 'inspect', IMAGE], { stdio: 'ignore' }).status === 0) return;
  execFileSync('deploy/images/sandbox/build.sh', [IMAGE, ''], {
    cwd: REPO_ROOT,
    env: { ...process.env, CONTAINER_ENGINE: 'docker' },
    stdio: 'inherit',
  });
}

function spawnBrain(binary: string, storage: string) {
  return spawn(binary, {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_MODEL_MODE: 'acp-container',
      AWAKEN_CONTAINER_IMAGE: IMAGE,
      SESSION_ENVIRONMENT_TIER: 'docker',
      AWAKEN_STORAGE_DIR: storage,
      AWAKEN_ACP_ARGV: `node -e ${ACP_FIXTURE}`,
      AWAKEN_SANDBOX_REAP_INTERVAL: '3600',
    },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
}

function sqlite(storage: string, statement: string): string {
  return execFileSync('sqlite3', ['-cmd', '.timeout 5000', path.join(storage, 'sessions.db'), statement], {
    encoding: 'utf8',
  }).trim();
}

function binding(storage: string, sessionId: string): Binding {
  const escaped = sessionId.replaceAll("'", "''");
  const aggregate = JSON.parse(sqlite(
    storage,
    `SELECT aggregate_json FROM managed_session WHERE session_id = '${escaped}'`,
  ));
  const encoded = aggregate.environment_binding;
  assert.ok(encoded, `Session ${sessionId} has a durable environment binding`);
  return JSON.parse(encoded);
}

function rewriteBinding(storage: string, sessionId: string, encoded: string): void {
  const id = sessionId.replaceAll("'", "''");
  const aggregate = JSON.parse(sqlite(
    storage,
    `SELECT aggregate_json FROM managed_session WHERE session_id = '${id}'`,
  ));
  aggregate.environment_binding = encoded;
  const value = JSON.stringify(aggregate).replaceAll("'", "''");
  // Cause/effect graph / decision table for durable corruption injection:
  // C1=root aggregate exists; C2=environment binding is damaged; C3=legacy
  // compatibility column differs. C1+C2 must drive recovery regardless of C3:
  // the aggregate is the sole authority and the old column is never dual-written.
  //
  // | Rule | aggregate binding | legacy column | recovery result |
  // | A1   | valid             | stale/null    | adopt          |
  // | A2   | corrupt           | any           | fail closed    |
  assert.equal(
    sqlite(
      storage,
      `UPDATE managed_session SET aggregate_json = '${value}' WHERE session_id = '${id}'; SELECT changes();`,
    ),
    '1',
  );
}

async function createRealizedSession(client: Anthropic, name: string): Promise<string> {
  const created = await client.beta.sessions.create({
    agent: 'assistant',
    metadata: { 'awaken.runtime': 'acp:custom', fault: name },
    environment_id: 'env_local',
    betas: BETAS,
  });
  await client.beta.sessions.events.send(created.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: `realize ${name}` }] }],
    betas: BETAS,
  });
  const observed: unknown[] = [];
  for await (const event of client.beta.sessions.events.list(created.id, { betas: BETAS })) {
    observed.push(event);
  }
  assert.ok(JSON.stringify(observed).includes(MARKER), `${name} realized its real container`);
  return created.id;
}

async function expectRestoreFailure(sessionId: string, marker: string): Promise<void> {
  const response = await fetch(`${BASE}/v1/sessions/${sessionId}/events`, {
    method: 'POST',
    headers: {
      'x-api-key': 'e2e-dummy',
      'anthropic-beta': BETAS.join(','),
      'content-type': 'application/json',
    },
    body: JSON.stringify({
      events: [{ type: 'user.message', content: [{ type: 'text', text: `restore ${marker}` }] }],
    }),
  });
  const body = await response.text();
  assert.equal(response.status, 500, `${marker} failed closed: ${response.status} ${body}`);
  assert.ok(body.includes('error'), `${marker} returned a structured error: ${body}`);
}

function removeContainers(ids: Iterable<string>): void {
  const unique = [...new Set([...ids].filter(Boolean))];
  if (unique.length > 0) spawnSync('docker', ['rm', '-f', ...unique], { stdio: 'ignore' });
}

function knownContainer(shortId: string, ...sets: Set<string>[]): boolean {
  return sets.some((set) => [...set].some((id) => id.startsWith(shortId)));
}

async function main(): Promise<void> {
  if (spawnSync('docker', ['version'], { stdio: 'ignore' }).status !== 0) {
    throw new Error('real Docker is required for Session recovery fault coverage');
  }
  ensureImage();
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-session-recovery-fault-'));
  const binary = buildBrain();
  const preexistingContainers = new Set(
    execFileSync(
      'docker',
      ['ps', '-q', '--filter', 'label=awaken.sandbox=1', '--filter', `ancestor=${IMAGE}`],
      { encoding: 'utf8' },
    ).trim().split(/\s+/).filter(Boolean),
  );
  let brain = spawnBrain(binary, storage);
  const containers = new Set<string>();
  try {
    await waitForPort(PORT, 180_000, brain);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

    const cases = new Map<string, string>();
    for (const name of [
      'invalid-json',
      'wrong-session',
      'wrong-provider',
      'missing-locator',
      'stopped-container',
      'deleted-container',
    ]) {
      const sessionId = await createRealizedSession(client, name);
      cases.set(name, sessionId);
      const containerId = binding(storage, sessionId).extra?.container_id;
      assert.ok(containerId, `${name} binding carries its physical container locator`);
      containers.add(containerId);
    }

    const killed = new Promise<void>((resolve) => brain.once('exit', () => resolve()));
    brain.kill('SIGKILL');
    await killed;

    rewriteBinding(storage, cases.get('invalid-json')!, '{not-json');

    const wrongSession = binding(storage, cases.get('wrong-session')!);
    wrongSession.sandbox_id = 'some-other-session';
    rewriteBinding(storage, cases.get('wrong-session')!, JSON.stringify(wrongSession));

    const wrongProvider = binding(storage, cases.get('wrong-provider')!);
    wrongProvider.provider_kind = 'namespace';
    rewriteBinding(storage, cases.get('wrong-provider')!, JSON.stringify(wrongProvider));

    const missingLocator = binding(storage, cases.get('missing-locator')!);
    delete missingLocator.extra;
    rewriteBinding(storage, cases.get('missing-locator')!, JSON.stringify(missingLocator));

    const stopped = binding(storage, cases.get('stopped-container')!).extra!.container_id!;
    execFileSync('docker', ['stop', stopped], { stdio: 'ignore' });
    const deleted = binding(storage, cases.get('deleted-container')!).extra!.container_id!;
    execFileSync('docker', ['rm', '-f', deleted], { stdio: 'ignore' });

    brain = spawnBrain(binary, storage);
    await waitForPort(PORT, 180_000, brain);
    for (const [name, sessionId] of cases) {
      await expectRestoreFailure(sessionId, name);
      const afterFault = execFileSync(
        'docker',
        ['ps', '-q', '--filter', 'label=awaken.sandbox=1', '--filter', `ancestor=${IMAGE}`],
        { encoding: 'utf8' },
      ).trim().split(/\s+/).filter(Boolean);
      assert.ok(
        afterFault.every((containerId) =>
          knownContainer(containerId, containers, preexistingContainers)),
        `${name} recovery created an unrelated replacement container: ${afterFault}`,
      );
    }

    // A corrupt durable identity must not be "recovered" by creating an unrelated
    // replacement container. The original four live containers remain the only
    // possible physical resources (one was stopped and one was deleted above).
    const live = execFileSync(
      'docker',
      ['ps', '-q', '--filter', 'label=awaken.sandbox=1', '--filter', `ancestor=${IMAGE}`],
      { encoding: 'utf8' },
    ).trim().split(/\s+/).filter(Boolean);
    assert.ok(
      live.every((containerId) => knownContainer(containerId, containers, preexistingContainers)),
      `restore failures created no replacement containers: ${live}`,
    );

    console.log(
      'SESSION ENVIRONMENT RECOVERY FAULT TS API E2E PASS: corrupt, cross-owner, wrong-provider, missing, stopped and deleted bindings fail closed.',
    );
  } finally {
    await stopServer(brain).catch(() => {});
    removeContainers(containers);
    fs.rmSync(storage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('SESSION ENVIRONMENT RECOVERY FAULT TS API E2E FAIL:', error);
  process.exitCode = 1;
});
