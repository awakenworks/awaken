// Crash/restart coverage for corrupt or unavailable Session-environment bindings.
//
// Every environment is first realized through the public Managed API and a real
// Docker daemon. The coordinator is then SIGKILLed, the disposable SQLite
// authority is damaged while offline, and a replacement is asked to restore each
// Session through the same public API. No test-only runtime hook is involved.

import assert from 'node:assert/strict';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
// @ts-ignore -- shared JavaScript harness intentionally serves TS scenarios.
import {
  assertPendingReceiptHasNoRuntimeEffects,
  REPO_ROOT,
  SKILLS_BETAS,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';
// @ts-ignore -- shared JavaScript SQLite fixture intentionally serves TS scenarios.
import { sqliteRun, sqliteScalar } from './sqlite.mjs';
// @ts-ignore -- shared Cargo artifact resolver intentionally serves TS scenarios.
import { cargoExecutable } from './cargo_binary.mjs';

const PORT = Number(process.env.E2E_PORT ?? 39774);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const IMAGE = process.env.AWAKEN_TEST_SESSION_IMAGE ?? 'awaken-sandbox:session-e2e';
const MARKER = 'RECOVERY-BINDING-OK';
const ACP_FIXTURE = `process.stdin.once('data',()=>{console.log(JSON.stringify({type:'message',text:'${MARKER}'}));console.log(JSON.stringify({type:'turn_end',reason:'natural_end'}))})`;

type Binding = {
  sandbox_id: string;
  payload: {
    schema: string;
    container_id?: string;
    provider_kind?: string;
    [key: string]: unknown;
  };
};

function buildBrain(): string {
  return cargoExecutable({
    cwd: REPO_ROOT,
    packageName: 'awaken-scenario-host',
    targetName: 'awaken-scenario-host',
    features: ['container-docker'],
  } as any);
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
      SESSION_DEPLOYMENT_STORAGE_DIR: storage,
      AWAKEN_ACP_ARGV: `node -e ${ACP_FIXTURE}`,
      AWAKEN_SANDBOX_REAP_INTERVAL: '3600',
    },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
}

function binding(storage: string, sessionId: string): Binding {
  const escaped = sessionId.replaceAll("'", "''");
  const aggregate = JSON.parse(String(sqliteScalar(
    path.join(storage, 'sessions.db'),
    `SELECT aggregate_json FROM managed_session WHERE session_id = '${escaped}'`,
  )));
  assert.equal(aggregate.format, 'awaken.session.v1');
  const environment = aggregate.aggregate.environment;
  assert.equal(environment?.phase, 'resident', `Session ${sessionId} is durably resident`);
  assert.ok(environment.binding, `Session ${sessionId} has a durable environment binding`);
  return JSON.parse(environment.binding);
}

function rewriteBinding(storage: string, sessionId: string, encoded: string): void {
  const database = path.join(storage, 'sessions.db');
  const preservedAggregate = sqliteScalar(
    database,
    `SELECT json_remove(aggregate_json, '$.aggregate.environment.binding')
       FROM managed_session WHERE session_id = ?`,
    sessionId,
  );
  assert.equal(typeof preservedAggregate, 'string', `Session ${sessionId} aggregate exists`);
  assert.equal(
    sqliteScalar(
      database,
      `SELECT json_extract(aggregate_json, '$.aggregate.environment.phase')
         FROM managed_session WHERE session_id = ?`,
      sessionId,
    ),
    'resident',
    `Session ${sessionId} remains resident while its binding is faulted`,
  );
  // Cause/effect graph / decision table for durable corruption injection:
  // C1=root aggregate exists; C2=its closed typed environment binding is
  // damaged; C3=the non-authoritative indexed projection differs; C4=all other
  // Resident facts (effect, generation, idle edge) remain exact. C1+C2+C4 must
  // drive recovery regardless of C3: the aggregate is the sole authority and
  // the indexed projection is never a fallback durable handle.
  //
  // | Rule | aggregate binding | other Resident facts | indexed projection | result |
  // | A1   | valid + available | preserved            | stale/null         | adopt |
  // | A2   | corrupt/foreign   | preserved            | any                | fail closed |
  // | A3   | valid + unavailable| preserved           | any                | fail closed |
  // A3 is distinct from a dispatch run explicitly pinned to
  // RebuildFromCommittedTruth; Managed Session restoration promises continuity.
  assert.equal(
    Number(sqliteRun(
      database,
      `UPDATE managed_session
         SET aggregate_json = json_set(aggregate_json, '$.aggregate.environment.binding', ?)
         WHERE session_id = ?`,
      encoded,
      sessionId,
    ).changes),
    1,
  );
  assert.equal(
    sqliteScalar(
      database,
      `SELECT json_extract(aggregate_json, '$.aggregate.environment.binding')
         FROM managed_session WHERE session_id = ?`,
      sessionId,
    ),
    encoded,
    `Session ${sessionId} stores the exact faulted binding`,
  );
  assert.equal(
    sqliteScalar(
      database,
      `SELECT json_remove(aggregate_json, '$.aggregate.environment.binding')
         FROM managed_session WHERE session_id = ?`,
      sessionId,
    ),
    preservedAggregate,
    `Session ${sessionId} preserves every non-binding aggregate fact`,
  );
}

async function createRealizedSession(client: Anthropic, name: string): Promise<string> {
  const created = await client.beta.sessions.create({
    agent: 'namespace-agent',
    metadata: { fault: name },
    environment_id: 'env_local',
    betas: BETAS,
  });
  // Realization decision rules: R1 accepted receipt unprocessed => wait; R2
  // exact receipt processed + later MARKER => durable container realized; R3
  // older markers or earlier-only history => ineligible for this Session Run.
  const receipt = await client.beta.sessions.events.send(created.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: `realize ${name}` }] }],
    betas: BETAS,
  });
  const acceptedId = receipt.data?.[0]?.id;
  assert.equal(typeof acceptedId, 'string', `${name} exact User Event receipt`);
  const observed = await waitForSessionEventReceipt(
    client,
    created.id,
    acceptedId,
    BETAS,
    ({ delta }: { delta: any[] }) => JSON.stringify(delta).includes(MARKER),
    `${name} Session container to commit its Agent marker`,
    { timeoutMs: 180_000, pollMs: 100 },
  );
  assert.ok(JSON.stringify(observed.delta).includes(MARKER), `${name} realized its real container`);
  return created.id;
}

async function expectRestoreFailure(
  client: Anthropic,
  sessionId: string,
  marker: string,
): Promise<void> {
  const listHistory = async (): Promise<any[]> => {
    const events: any[] = [];
    for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
      events.push(event);
    }
    return events;
  };
  // The Session already contains the successful realization command. Freeze
  // that committed baseline before admitting the restoration attempt so the
  // negative oracle cannot mistake old Agent/usage/idle events for new effects.
  const priorHistory = await listHistory();
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
  // Durable-ingress outcome decision table: C1 invalid wire/admission state ->
  // synchronous 500; C2 the Session-root command is durably accepted before
  // the Worker observes a retryable restoration fault -> 200 exact receipt that
  // remains unprocessed, with no Agent/model/tool effect. Both fail closed; C2
  // must remain retryable rather than being compensated or falsely terminated.
  //
  // | Rule | Admission | Runtime restore | Observable outcome |
  // | F1 | reject | not run | HTTP 500 error |
  // | F2 | accept | corrupt/unavailable | HTTP 200 receipt; one pending list receipt |
  if (response.status === 500) {
    assert.ok(body.includes('error'), `${marker} returned a structured error: ${body}`);
    return;
  }
  assert.equal(response.status, 200, `${marker} admission shape: ${response.status} ${body}`);
  const accepted = JSON.parse(body).data?.[0];
  const acceptedId = accepted?.id;
  assert.equal(typeof acceptedId, 'string', `${marker} exact User Event receipt`);
  assert.equal(accepted.processed_at, null, `${marker} effect failure is not falsely processed`);
  // Pending custody is observed over one bounded reconciliation window. Waiting
  // for a terminal event would be the wrong oracle because retryable custody
  // intentionally remains pending until the damaged binding is repaired.
  await new Promise((resolve) => setTimeout(resolve, 750));
  const observed = await listHistory();
  assertPendingReceiptHasNoRuntimeEffects({
    history: observed,
    priorHistory,
    receiptId: acceptedId,
    forbiddenEventTypes: new Set([
      'agent.message',
      'agent.tool_use',
      'agent.tool_result',
      'session.error',
      'session.status_idle',
      'session.usage',
      'span.model_request_start',
      'span.model_request_end',
    ]),
    description: `${marker} retryable restoration fault`,
  });
}

function removeContainers(ids: Iterable<string>): void {
  const unique = [...new Set([...ids].filter(Boolean))];
  if (unique.length > 0) spawnSync('docker', ['rm', '-f', ...unique], { stdio: 'ignore' });
}

function knownContainer(shortId: string, ...sets: Set<string>[]): boolean {
  return sets.some((set) => [...set].some((id) => id.startsWith(shortId)));
}

async function main(): Promise<void> {
  // Test design (Session Environment recovery faults). Causes: C1=a real
  // container-backed Session has one closed `container_v1` durable handle;
  // C2=after a crash that handle is malformed JSON, names a foreign Session,
  // contains a structurally valid foreign-provider payload, omits the exact
  // container locator, or names a stopped/deleted container; C3=the replacement
  // process restores from the aggregate; C4=the fault is observed before or
  // after durable Event admission. Effects: E1=every C2 arm fails closed before
  // model/tool execution; E2a=pre-admission observation returns structured 500,
  // or E2b=post-admission observation retains the exact unprocessed User receipt
  // with no terminal effect; E3=no replacement or unrelated container is
  // created. Constraint K: the aggregate's exact typed handle,
  // Session owner, provider payload, and live physical locator must all agree;
  // recovery never guesses, synthesizes, or falls back to an indexed column.
  // Decision rules F1-F6 pair C1+C3 with each C2 arm and require E1+E3 plus
  // E2a/E2b according to C4; acknowledgement never manufactures completion.
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
    await waitForPort(PORT, 180_000, brain as any);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

    // Cause graph / decision table for the immutable Agent fixture:
    // published skill id + absent Workspace resource -> publication validation
    // fails before realization; published id + present resource -> the recovery
    // cases reach their intended Session-environment boundary.
    //
    // | skill beta | publication skill  | Workspace skill | setup result     |
    // | absent     | any                | any             | reject route     |
    // | present    | delivered-container| absent          | reject publish   |
    // | present    | delivered-container| present         | realize container|
    await client.beta.skills.create({
      files: [await toFile(Buffer.from(
        '---\nname: delivered-container\ndescription: recovery fixture skill\n---\nRECOVERY-SKILL-OK',
      ), 'SKILL.md')],
      betas: SKILLS_BETAS,
    });

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
      const durableBinding = binding(storage, sessionId);
      assert.equal(
        durableBinding.payload.schema,
        'container_v1',
        `${name} binding uses the canonical container payload`,
      );
      const containerId = durableBinding.payload.container_id;
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
    wrongProvider.payload = { schema: 'unmanaged', provider_kind: 'bwrap' };
    rewriteBinding(storage, cases.get('wrong-provider')!, JSON.stringify(wrongProvider));

    const missingLocator = binding(storage, cases.get('missing-locator')!);
    delete missingLocator.payload.container_id;
    rewriteBinding(storage, cases.get('missing-locator')!, JSON.stringify(missingLocator));

    const stopped = binding(storage, cases.get('stopped-container')!).payload.container_id!;
    execFileSync('docker', ['stop', stopped], { stdio: 'ignore' });
    const deleted = binding(storage, cases.get('deleted-container')!).payload.container_id!;
    execFileSync('docker', ['rm', '-f', deleted], { stdio: 'ignore' });

    brain = spawnBrain(binary, storage);
    await waitForPort(PORT, 180_000, brain as any);
    for (const [name, sessionId] of cases) {
      await expectRestoreFailure(client, sessionId, name);
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
