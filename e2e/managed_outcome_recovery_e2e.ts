// Managed Outcome lifecycle fault/recovery E2E through the official Anthropic
// TypeScript SDK. The tests use real server processes and a real provider socket;
// only the upstream model response is deterministic.

import assert from 'node:assert/strict';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const clientFor = (baseURL) => new Anthropic({ apiKey: 'e2e-dummy', baseURL });

async function createSession(client) {
  return client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
}

async function defineOutcome(client, sessionId, rubric, maxIterations = 3) {
  return client.beta.sessions.events.send(sessionId, {
    events: [{
      type: 'user.define_outcome',
      description: 'produce the final deliverable',
      rubric: { type: 'text', content: rubric },
      max_iterations: maxIterations,
    }],
    betas: BETAS,
  });
}

async function listEvents(client, sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return events;
}

async function waitUntil(predicate, label, timeoutMs = 15_000) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error(`timed out waiting for ${label}`);
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}

async function interruptAt(port, requestOrdinal, rubric, maxIterations, phase) {
  const upstream = await startUpstream('revise', { delayMs: 700 });
  const spawned = spawnServer('outcome-matrix', port, {
    ...realServerEnv('revise', upstream, { mode: 'outcome-matrix' }),
    AWAKEN_OUTCOME_JUDGE_RUNTIME: 'native',
  });
  try {
    await waitForPort(port);
    const client = clientFor(spawned.baseUrl);
    const session = await createSession(client);
    const pending = defineOutcome(client, session.id, rubric, maxIterations);
    await waitUntil(() => upstream.received >= requestOrdinal, `${phase} inference`);
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.interrupt' }],
      betas: BETAS,
    });
    await pending;
    const events = await listEvents(client, session.id);
    const ends = events.filter((event) => event.type === 'span.outcome_evaluation_end');
    assert.equal(ends.at(-1)?.result, 'interrupted', `${phase} did not project interruption`);
    pass(`user.interrupt cancels an Outcome during ${phase}`);
  } finally {
    await stopServer(spawned.server).catch(() => {});
    upstream.close();
  }
}

async function judgeDecisionAndSchemaPaths(port) {
  const upstream = await startUpstream('revise');
  const spawned = spawnServer('outcome-matrix', port, {
    ...realServerEnv('revise', upstream, { mode: 'outcome-matrix' }),
    AWAKEN_OUTCOME_JUDGE_RUNTIME: 'native',
  });
  try {
    await waitForPort(port);
    const client = clientFor(spawned.baseUrl);

    const failed = await createSession(client);
    await defineOutcome(client, failed.id, 'FORCE_FAILED_DECISION');
    const failedEnds = (await listEvents(client, failed.id))
      .filter((event) => event.type === 'span.outcome_evaluation_end');
    assert.deepEqual(failedEnds.map((event) => event.result), ['failed']);

    const invalid = await createSession(client);
    const before = upstream.received;
    await assert.rejects(
      defineOutcome(client, invalid.id, 'INVALID_JUDGE_OUTPUT'),
      (error) => typeof error?.status === 'number' && error.status >= 500,
      'invalid Judge JSON must fail closed',
    );
    const afterFailure = upstream.received;
    assert.ok(afterFailure >= before + 2, 'Worker and Judge both ran before schema rejection');
    await assert.rejects(
      defineOutcome(client, invalid.id, 'INVALID_JUDGE_OUTPUT'),
      (error) => typeof error?.status === 'number' && error.status >= 500,
      'a new explicit command may start a new Outcome after the terminal error',
    );
    assert.ok(
      upstream.received > afterFailure,
      'the second explicit define command starts a distinct attempt after terminal cleanup',
    );
    pass('Judge failed decision and invalid strict JSON both project fail-closed semantics');
  } finally {
    await stopServer(spawned.server).catch(() => {});
    upstream.close();
  }
}

async function executionFailurePath(port, failedKind) {
  const upstream = await startUpstream('revise', {
    failArrivalKind: failedKind,
    faultStatus: 400,
  });
  const spawned = spawnServer('outcome-matrix', port, {
    ...realServerEnv('revise', upstream, { mode: 'outcome-matrix' }),
    AWAKEN_OUTCOME_JUDGE_RUNTIME: 'native',
  });
  try {
    await waitForPort(port);
    const client = clientFor(spawned.baseUrl);
    const session = await createSession(client);
    await assert.rejects(
      defineOutcome(client, session.id, 'FINAL'),
      (error) => typeof error?.status === 'number' && error.status >= 500,
      `${failedKind} provider failure must surface as infrastructure failure`,
    );
    assert.ok(upstream.arrivals.includes(failedKind), `${failedKind} was not exercised`);
    const ends = (await listEvents(client, session.id))
      .filter((event) => event.type === 'span.outcome_evaluation_end');
    assert.ok(
      ends.every((event) => event.result !== 'failed'),
      `${failedKind} infrastructure failure must not become a rubric decision`,
    );
    pass(`${failedKind} provider failure remains an infrastructure failure`);
  } finally {
    await stopServer(spawned.server).catch(() => {});
    upstream.close();
  }
}

async function recoverAfterJudgeCrash(port) {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-outcome-recovery-'));
  const upstream = await startUpstream('revise', { delayMs: 700 });
  const environment = {
    ...realServerEnv('revise', upstream, { mode: 'outcome-matrix' }),
    AWAKEN_OUTCOME_JUDGE_RUNTIME: 'native',
    SESSION_DEPLOYMENT_STORAGE_DIR: storage,
  };
  let spawned = spawnServer('outcome-matrix', port, environment);
  try {
    await waitForPort(port);
    let client = clientFor(spawned.baseUrl);
    const session = await createSession(client);
    const pending = defineOutcome(client, session.id, 'FINAL').then(
      () => ({ error: null }),
      (error) => ({ error }),
    );
    await waitUntil(() => upstream.received >= 2, 'first Judge inference');
    const exited = new Promise((resolve) => spawned.server.once('exit', resolve));
    spawned.server.kill('SIGKILL');
    await exited;
    assert.ok((await pending).error, 'the request in the killed process must disconnect');
    const receivedAtCrash = upstream.received;
    assert.equal(receivedAtCrash, 2, 'crash occurs after Worker commit and during Judge inference');

    spawned = spawnServer('outcome-matrix', port, {
      ...environment,
      // Deliberately change current configuration. Recovery must use the Native
      // Judge snapshot persisted in the active Outcome binding, not this ACP
      // default selected by the restarted process.
      AWAKEN_OUTCOME_JUDGE_RUNTIME: 'acp',
    });
    await waitForPort(port);
    client = clientFor(spawned.baseUrl);
    await defineOutcome(client, session.id, 'FINAL');
    const ends = (await listEvents(client, session.id))
      .filter((event) => event.type === 'span.outcome_evaluation_end');
    assert.deepEqual(ends.map((event) => event.result), ['needs_revision', 'satisfied']);
    assert.equal(
      upstream.arrivals.filter((kind) => kind === 'outcome-initial').length,
      1,
      `recovery must not repeat the committed initial Worker: ${upstream.arrivals}`,
    );
    assert.equal(
      upstream.arrivals.filter((kind) => kind === 'outcome-revision').length,
      1,
      `recovery runs exactly one revision Worker: ${upstream.arrivals}`,
    );
    assert.ok(
      upstream.arrivals.filter((kind) => kind === 'outcome-judge').length >= 3,
      `the interrupted Judge plus two recovered evaluations are observable: ${upstream.arrivals}`,
    );
    pass('SIGKILL recovery uses Thread truth and its pinned Judge snapshot');
  } finally {
    await stopServer(spawned.server).catch(() => {});
    upstream.close();
  }
}

async function main() {
  await interruptAt(39541, 1, 'FINAL', 3, 'Worker');
  await interruptAt(39542, 2, 'FINAL', 3, 'Judge');
  await interruptAt(39543, 3, 'NEVER_PRESENT_TOKEN', 1, 'acknowledgment');
  await judgeDecisionAndSchemaPaths(39544);
  await executionFailurePath(39546, 'outcome-initial');
  await executionFailurePath(39547, 'outcome-judge');
  await recoverAfterJudgeCrash(39545);
  console.log('E2E PASS: Managed Outcome interruption, Judge failures, and crash recovery.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
