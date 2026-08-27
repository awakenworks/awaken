// Managed Outcome lifecycle fault/recovery E2E through the official Anthropic
// TypeScript SDK. The tests use real server processes and a real provider socket;
// only the upstream model response is deterministic.
//
// Cause/effect graph: C1=a retained DefineOutcome owns one stable command/id;
// C2=foreground and lifecycle supervisor prepare C1 concurrently; C3=a distinct
// DefineOutcome arrives while C1 is active; C4=the process dies after a Worker
// commit and during Judge execution; C5=terminal/inactive aggregates are scanned
// after restart; C6=User interrupt targets an active Outcome; C7=Judge decision,
// schema, or Provider execution fails. Effects: E1=C1 is processed without
// duplicate execution; E2=C3 remains unprocessed/retryable until C1 terminates;
// E3=C4 resumes the exact id/binding without another define or repeated Worker;
// E4=C5 causes no new work; E5=C6 projects one interrupted terminal report;
// its exact interrupt receipt returns without any non-interrupted terminal report;
// E6=decision failure is a terminal report, while infrastructure/schema failures
// project no partial Outcome spans. Decision table:
// | Rule | Cause | Effect |
// | R1 | C1+C2 | E1 one Outcome, one initial Worker |
// | R2 | C1+C3 | E2 then two ordered terminal Outcomes |
// | R3 | C1+C4 | E3 |
// | R4 | C5 terminal/inactive | E4 |
// | R5 | C1+C6 at Worker/Judge/ack | E5 |
// | R6 | C7 decision / infrastructure | failed report / E6 |
// Constraints/invariant: one retained command/id and committed Thread truth own
// recovery; replay, concurrent supervisors, and interrupts may not duplicate
// Worker/Judge execution or project partial terminal spans.

import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import type {
  BetaManagedAgentsSendSessionEvents,
  BetaManagedAgentsSessionEvent,
  BetaManagedAgentsUserDefineOutcomeEvent,
} from '@anthropic-ai/sdk/resources/beta/sessions/events';
// @ts-ignore -- shared JS harness deliberately serves both JS and TS scenarios.
import { committedEffectsAfterUnanchoredReceipt, pass, realServerEnv, spawnServer, startUpstream, stopServer, waitForPort, waitForSessionEventReceipt, waitForValue } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const BASE_PORT = Number(process.env.E2E_PORT);
const clientFor = (baseURL: string) => new Anthropic({ apiKey: 'e2e-dummy', baseURL });
type OutcomeEndEvent = Extract<BetaManagedAgentsSessionEvent, { type: 'span.outcome_evaluation_end' }>;
type FailedArrivalKind = 'outcome-initial' | 'outcome-judge';

function outcomeEnds(
  events: BetaManagedAgentsSessionEvent[],
  outcomeId?: string,
): OutcomeEndEvent[] {
  return events.filter((event): event is OutcomeEndEvent =>
    event.type === 'span.outcome_evaluation_end'
      && (outcomeId === undefined || event.outcome_id === outcomeId));
}

async function createSession(client: Anthropic) {
  return client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
}

async function defineOutcome(
  client: Anthropic,
  sessionId: string,
  rubric: string,
  maxIterations = 3,
) {
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

async function listEvents(client: Anthropic, sessionId: string): Promise<BetaManagedAgentsSessionEvent[]> {
  const events: BetaManagedAgentsSessionEvent[] = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return events;
}

function outcomeReceipt(receipt: BetaManagedAgentsSendSessionEvents): BetaManagedAgentsUserDefineOutcomeEvent {
  const accepted = receipt.data?.[0];
  assert.equal(accepted?.type, 'user.define_outcome', 'official SDK returns one DefineOutcome receipt');
  if (accepted?.type !== 'user.define_outcome') throw new Error('Outcome receipt has the wrong event family');
  assert.equal(
    accepted.processed_at,
    null,
    'a new DefineOutcome returns its exact durable receipt before lifecycle execution',
  );
  return accepted;
}

async function terminalOutcomeEvents(
  client: Anthropic,
  sessionId: string,
  receiptId: string,
  outcomeId: string,
  result?: OutcomeEndEvent['result'],
): Promise<BetaManagedAgentsSessionEvent[]> {
  // Terminal recovery rule: C1=an exact retained receipt and C2=its terminal
  // Outcome end; E=return committed history. K=outcome_id scopes the delta.
  // R=C1+C2=>E; missing/unprocessed receipt or missing terminal end=>retry.
  const { events }: { events: BetaManagedAgentsSessionEvent[] } = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta }: { delta: BetaManagedAgentsSessionEvent[] }) => delta.some((event) =>
      event.type === 'span.outcome_evaluation_end'
        && event.outcome_id === outcomeId
        && (result === undefined || event.result === result)),
    `Outcome ${outcomeId} to reach its terminal projection`,
    { timeoutMs: 30_000, pollMs: 50 },
  );
  return events;
}

async function interruptAt(
  port: number,
  requestOrdinal: number,
  rubric: string,
  maxIterations: number,
  phase: string,
) {
  const upstream = await startUpstream('revise', { delayMs: 700 });
  const spawned = spawnServer('outcome-matrix', port, {
    ...realServerEnv('revise', upstream, { mode: 'outcome-matrix' }),
    AWAKEN_OUTCOME_JUDGE_RUNTIME: 'native',
  });
  try {
    await waitForPort(port);
    const client = clientFor(spawned.baseUrl);
    const session = await createSession(client);
    const accepted = outcomeReceipt(await defineOutcome(client, session.id, rubric, maxIterations));
    await waitForValue(
      () => upstream.received,
      (received: number) => received >= requestOrdinal,
      `${phase} inference`,
      { timeoutMs: 15_000, pollMs: 20 },
    );
    const interruptReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.interrupt' }],
      betas: BETAS,
    });
    const immediatelyAfterReceipt = await listEvents(client, session.id);
    const acceptedInterrupt = interruptReceipt.data?.[0];
    assert.equal(acceptedInterrupt?.type, 'user.interrupt', `${phase} exact interrupt receipt family`);
    if (acceptedInterrupt?.type !== 'user.interrupt') {
      throw new Error(`${phase} interrupt receipt has the wrong event family`);
    }
    assert.ok(
      !outcomeEnds(immediatelyAfterReceipt, accepted.outcome_id).some((event) =>
        event.result !== 'interrupted'),
      `${phase} interrupt receipt must precede a non-interrupted terminal Outcome`,
    );
    // Interrupt projection rule: C1 the exact interrupt receipt is processed;
    // C2 the synthetic interrupted cycle is anchored by the same terminal
    // commit and canonical ordering may place C2 before C1. E1 both facts are
    // present in committed history; E2 exactly one terminal is interrupted and
    // no non-interrupted terminal is manufactured. I1 C1+C2=>E1+E2. Receipt
    // delta is not a causal boundary for this terminal-owned projection.
    const events = await waitForValue(
      () => listEvents(client, session.id),
      (history) => history.some((event) =>
        event.id === acceptedInterrupt.id && event.processed_at != null)
        && outcomeEnds(history, accepted.outcome_id).some((event) => event.result === 'interrupted'),
      `${phase} interrupt receipt and terminal projection`,
      { timeoutMs: 30_000, pollMs: 50 },
    );
    const ends = outcomeEnds(events, accepted.outcome_id);
    assert.deepEqual(
      ends.map((event) => event.result),
      ['interrupted'],
      `${phase} projects one interrupted terminal and no competing result`,
    );
    pass(`user.interrupt cancels an Outcome during ${phase}`);
  } finally {
    await stopServer(spawned.server).catch(() => {});
    upstream.close();
  }
}

async function judgeDecisionAndSchemaPaths(port: number) {
  const upstream = await startUpstream('revise');
  const spawned = spawnServer('outcome-matrix', port, {
    ...realServerEnv('revise', upstream, { mode: 'outcome-matrix' }),
    AWAKEN_OUTCOME_JUDGE_RUNTIME: 'native',
  });
  try {
    await waitForPort(port);
    const client = clientFor(spawned.baseUrl);

    const failed = await createSession(client);
    const failedAccepted = outcomeReceipt(
      await defineOutcome(client, failed.id, 'FORCE_FAILED_DECISION'),
    );
    const failedEnds = (await terminalOutcomeEvents(
      client,
      failed.id,
      failedAccepted.id,
      failedAccepted.outcome_id,
      'failed',
    )).filter((event): event is OutcomeEndEvent =>
      event.type === 'span.outcome_evaluation_end'
        && event.outcome_id === failedAccepted.outcome_id);
    assert.deepEqual(failedEnds.map((event) => event.result), ['failed']);

    const invalid = await createSession(client);
    const before = upstream.received;
    const invalidAccepted = outcomeReceipt(
      await defineOutcome(client, invalid.id, 'INVALID_JUDGE_OUTPUT'),
    );
    // Invalid-Judge rule: C1=exact definition receipt and C2=both Provider
    // attempts occurred; E=later idle with no partial Outcome spans. K=the
    // receipt delta excludes older idle. R=C1+C2+E=>inspect fail-closed history.
    const { events: invalidEvents }: { events: BetaManagedAgentsSessionEvent[] } = await waitForSessionEventReceipt(
      client,
      invalid.id,
      invalidAccepted.id,
      BETAS,
      ({ delta }: { delta: BetaManagedAgentsSessionEvent[] }) => upstream.received >= before + 2
        && delta.some((event) => event.type === 'session.status_idle'),
      'invalid Judge Worker/Judge attempts and retained command',
    );
    assert.ok(
      !invalidEvents.some((event) =>
        event.type.startsWith('span.outcome_evaluation_')
          && 'outcome_id' in event
          && event.outcome_id === invalidAccepted.outcome_id),
      'R6 infrastructure/schema failure projects no partial Outcome spans',
    );
    pass('Judge failed decision and invalid strict JSON both project fail-closed semantics');
  } finally {
    await stopServer(spawned.server).catch(() => {});
    upstream.close();
  }
}

async function executionFailurePath(port: number, failedKind: FailedArrivalKind) {
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
    const accepted = outcomeReceipt(await defineOutcome(client, session.id, 'FINAL'));
    // Provider-failure rule: C1=exact definition receipt and C2=the selected
    // upstream arrival fails; E=inspect committed history. K=Provider failure
    // is not a rubric decision. R=C1+C2=>E; otherwise retry boundedly.
    const { events }: { events: BetaManagedAgentsSessionEvent[] } = await waitForSessionEventReceipt(
      client,
      session.id,
      accepted.id,
      BETAS,
      () => upstream.arrivals.includes(failedKind),
      `${failedKind} provider failure to occur after durable acceptance`,
    );
    assert.ok(upstream.arrivals.includes(failedKind), `${failedKind} was not exercised`);
    const ends = outcomeEnds(events);
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

async function competingOutcomeCommands(port: number) {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-outcome-competing-'));
  const upstream = await startUpstream('revise', { delayMs: 350 });
  const spawned = spawnServer('outcome-matrix', port, {
    ...realServerEnv('revise', upstream, { mode: 'outcome-matrix' }),
    AWAKEN_OUTCOME_JUDGE_RUNTIME: 'native',
    SESSION_DEPLOYMENT_STORAGE_DIR: storage,
  });
  try {
    await waitForPort(port);
    const client = clientFor(spawned.baseUrl);
    const session = await createSession(client);

    // R2/F2: the first retained command is in a real Provider request when the
    // second distinct command is accepted. Busy is retryable internal state:
    // the second receipt remains unprocessed and absent from committed history,
    // with no Outcome spans until the first aggregate terminates; the sole
    // supervisor then anchors and advances it.
    const first = outcomeReceipt(await defineOutcome(client, session.id, 'FINAL', 3));
    await waitForValue(
      () => upstream.received,
      (received: number) => received >= 1,
      'R2 first Outcome Worker to be active',
      { timeoutMs: 15_000, pollMs: 20 },
    );
    const beforeSecond = await listEvents(client, session.id);
    const second = outcomeReceipt(
      await defineOutcome(client, session.id, 'NEVER_PRESENT_TOKEN', 1),
    );
    assert.notEqual(second.outcome_id, first.outcome_id, 'R2 competing root commands have distinct ids');
    assert.equal(second.processed_at, null, 'R2 Busy leaves the competing root Event unprocessed');
    const whileBusy = await listEvents(client, session.id);
    committedEffectsAfterUnanchoredReceipt({
      history: whileBusy,
      priorHistory: beforeSecond,
      receiptId: second.id,
      forbiddenEventTypes: new Set(),
      description: 'R2 Busy competing Outcome',
    });
    assert.equal(outcomeEnds(whileBusy, second.outcome_id).length, 0, 'R2 no premature competing report');

    await terminalOutcomeEvents(client, session.id, first.id, first.outcome_id, 'satisfied');
    const completed = await terminalOutcomeEvents(
      client,
      session.id,
      second.id,
      second.outcome_id,
      'max_iterations_reached',
    );
    const retainedSecond = completed.find((event) => event.id === second.id);
    assert.ok(retainedSecond?.processed_at, 'R2 supervisor retries and processes the competing command');
    assert.deepEqual(
      outcomeEnds(completed, first.outcome_id).map((event) => event.result),
      ['needs_revision', 'satisfied'],
      'R1/F1 foreground+supervisor replay commits one first Outcome report',
    );
    assert.deepEqual(
      outcomeEnds(completed, second.outcome_id).map((event) => event.result),
      ['max_iterations_reached'],
      'R2/F2 competing Outcome runs exactly once after the active owner clears',
    );
    assert.equal(
      (upstream.arrivals as string[]).filter((kind) => kind === 'outcome-initial').length,
      2,
      `R1-R2 each retained command executes one initial Worker: ${upstream.arrivals}`,
    );
    pass('same-command replay deduplicates; competing Outcome stays retryable then runs once');
  } finally {
    await stopServer(spawned.server).catch(() => {});
    upstream.close();
    rmSync(storage, { recursive: true, force: true });
  }
}

async function recoverAfterJudgeCrash(port: number) {
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
    const inactive = await createSession(client);
    const session = await createSession(client);
    const accepted = outcomeReceipt(await defineOutcome(client, session.id, 'FINAL'));
    await waitForValue(
      () => upstream.received,
      (received: number) => received >= 2,
      'first Judge inference',
      { timeoutMs: 15_000, pollMs: 20 },
    );
    const exited = new Promise<void>((resolve) => spawned.server.once('exit', () => resolve()));
    spawned.server.kill('SIGKILL');
    await exited;
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
    const recoveredEvents = await terminalOutcomeEvents(
      client,
      session.id,
      accepted.id,
      accepted.outcome_id,
      'satisfied',
    );
    const ends = outcomeEnds(recoveredEvents, accepted.outcome_id);
    const arrivals: string[] = upstream.arrivals;
    assert.deepEqual(ends.map((event) => event.result), ['needs_revision', 'satisfied']);
    assert.equal(
      arrivals.filter((kind) => kind === 'outcome-initial').length,
      1,
      `recovery must not repeat the committed initial Worker: ${upstream.arrivals}`,
    );
    assert.equal(
      arrivals.filter((kind) => kind === 'outcome-revision').length,
      1,
      `recovery runs exactly one revision Worker: ${upstream.arrivals}`,
    );
    assert.ok(
      arrivals.filter((kind) => kind === 'outcome-judge').length >= 3,
      `the interrupted Judge plus two recovered evaluations are observable: ${upstream.arrivals}`,
    );
    assert.equal(
      recoveredEvents.filter((event) => event.type === 'user.define_outcome').length,
      1,
      'R3 recovery resumes retained provenance without a second DefineOutcome',
    );

    // R4/C2-C3: restart once more after terminal settlement. The completed
    // aggregate and an unrelated inactive Session must drive no Provider work.
    const receivedAtTerminal = upstream.received;
    await stopServer(spawned.server);
    spawned = spawnServer('outcome-matrix', port, environment);
    await waitForPort(port);
    client = clientFor(spawned.baseUrl);
    const replayed = await terminalOutcomeEvents(
      client,
      session.id,
      accepted.id,
      accepted.outcome_id,
      'satisfied',
    );
    await client.beta.sessions.retrieve(inactive.id, { betas: BETAS });
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert.equal(upstream.received, receivedAtTerminal, 'R4 terminal/inactive restart performs no work');
    assert.deepEqual(
      replayed.filter((event) => event.type.startsWith('span.outcome_evaluation_')).map((event) => event.id),
      recoveredEvents.filter((event) => event.type.startsWith('span.outcome_evaluation_')).map((event) => event.id),
      'R4 terminal replay preserves stable span ids without duplication',
    );
    pass('SIGKILL recovery and terminal/inactive replay use durable Thread truth');
  } finally {
    await stopServer(spawned.server).catch(() => {});
    upstream.close();
    rmSync(storage, { recursive: true, force: true });
  }
}

async function main() {
  // Port-ownership cause/effect table: C1 the harness assigns a process-local
  // low port block; C2 an old fixed 39xxx port overlaps Linux's ephemeral
  // client range; C3 recovery must restart on the exact same address. Effects:
  // E1 every independent arm receives a stable non-ephemeral offset, E2 no
  // preceding upstream connection can occupy the next server address, and E3
  // the crash arm can still prove same-address recovery.
  //
  // | Rule | Address source | Reuse | Effect |
  // |---|---|---|---|
  // | P1 | harness base + unique offset | no | isolated scenario bind |
  // | P2 | harness base + recovery offset | yes | exact restart bind |
  // | P3 | fixed ephemeral-range neighbor | any | forbidden EADDRINUSE race |
  // Constraints/invariant: each arm owns a disjoint harness offset except the
  // intentional same-address crash/restart pair.
  await interruptAt(BASE_PORT + 1, 1, 'FINAL', 3, 'Worker');
  await interruptAt(BASE_PORT + 2, 2, 'FINAL', 3, 'Judge');
  await interruptAt(BASE_PORT + 3, 3, 'NEVER_PRESENT_TOKEN', 1, 'acknowledgment');
  await judgeDecisionAndSchemaPaths(BASE_PORT + 4);
  await executionFailurePath(BASE_PORT + 6, 'outcome-initial');
  await executionFailurePath(BASE_PORT + 7, 'outcome-judge');
  await competingOutcomeCommands(BASE_PORT + 8);
  await recoverAfterJudgeCrash(BASE_PORT + 5);
  console.log('E2E PASS: Managed Outcome interruption, Judge failures, and crash recovery.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
