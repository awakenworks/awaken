// Managed Outcome end-to-end backend matrix through the official Anthropic TS
// SDK. Every Worker/Judge Native×ACP pairing must use the same Run interface and
// produce the same zero-based lifecycle.
//
// Cause/effect graph: C1=Worker backend is Native/ACP; C2=Judge backend is
// Native/ACP; C3=definition uses Text/File rubric; C4=max_iterations is omitted,
// null, or explicit. Effects: E1=all backend pairs project the same ordered
// Outcome lifecycle; E2=the retained public definition preserves C3/C4 exactly;
// E3=only execution maps omitted/null to default 3; E4=explicit limits and input
// boundaries are enforced. Decision table:
// | Rule | Worker/Judge | Rubric | max | Effect |
// | M1-M4 | Native/ACP cross product | Text | 3 | E1 satisfied at iteration 1 |
// | P1 | Native | Text | omitted | E2 null on wire; E3 three Grades |
// | P2 | Native | Text | null | E2 null on wire; E3 three Grades |
// | P3 | Native | File | 2 | E2 exact File id + 2; E4 two Grades |
// | B1 | ACP | Text | 1 | E4 one Grade + ungraded acknowledgment |
// | B2 | any | blank/outside 1..20 | any | 400 before persistence |
// Constraints/invariant: Worker and Judge vary only behind the shared Run
// interface; one retained definition and zero-based lifecycle remain identical.

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import type {
  BetaManagedAgentsFileRubricParams,
  BetaManagedAgentsSessionEvent,
  BetaManagedAgentsTextRubricParams,
  BetaManagedAgentsUserDefineOutcomeEventParams,
} from '@anthropic-ai/sdk/resources/beta/sessions/events';
// @ts-ignore -- shared JS harness deliberately serves both JS and TS scenarios.
import { FILES_BETA, pass, waitForSessionEventReceipt, withScenarioServer } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01', FILES_BETA];

type OutcomeRubric = BetaManagedAgentsTextRubricParams | BetaManagedAgentsFileRubricParams;
type RuntimeKind = 'native' | 'acp';
type OutcomeEndEvent = Extract<BetaManagedAgentsSessionEvent, { type: 'span.outcome_evaluation_end' }>;

function outcomeEnds(events: BetaManagedAgentsSessionEvent[], outcomeId?: string): OutcomeEndEvent[] {
  return events.filter((event): event is OutcomeEndEvent =>
    event.type === 'span.outcome_evaluation_end'
      && (outcomeId === undefined || event.outcome_id === outcomeId));
}

async function sendMessage(client: Anthropic, sessionId: string, text: string) {
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const accepted = receipt.data?.[0];
  assert.ok(accepted, 'official SDK returns one retained User receipt');
  // Seed rule: C=exact User receipt; E=a later message and end-turn idle.
  // K=older transcript events cannot settle it. R=C+E=>seed Run committed.
  return waitForSessionEventReceipt(
    client,
    sessionId,
    accepted.id,
    BETAS,
    ({ delta }: { delta: BetaManagedAgentsSessionEvent[] }) => delta.some((event) => event.type === 'agent.message')
      && [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn',
    'the seed User Run to commit',
  );
}

async function defineOutcome(
  client: Anthropic,
  sessionId: string,
  rubric: OutcomeRubric,
  maxIterations: number | null | undefined,
) {
  const input: BetaManagedAgentsUserDefineOutcomeEventParams = {
    type: 'user.define_outcome',
    description: 'produce the final deliverable',
    rubric,
    ...(maxIterations === undefined ? {} : { max_iterations: maxIterations }),
  };
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [input],
    betas: BETAS,
  });
  const accepted = receipt.data?.[0];
  assert.equal(accepted?.type, 'user.define_outcome', 'official SDK returns the retained Outcome command');
  if (accepted?.type !== 'user.define_outcome') throw new Error('Outcome receipt has the wrong event family');
  // Outcome rule: C=exact retained definition; E=its non-revision terminal end.
  // K=receipt+outcome_id own the scope. R=C+E=>return the full transcript oracle.
  const { events }: { events: BetaManagedAgentsSessionEvent[] } = await waitForSessionEventReceipt(
    client,
    sessionId,
    accepted.id,
    BETAS,
    ({ delta }: { delta: BetaManagedAgentsSessionEvent[] }) => delta.some((event) =>
        event.type === 'span.outcome_evaluation_end'
          && event.outcome_id === accepted.outcome_id
          && event.result !== 'needs_revision'),
    'the retained Outcome to reach a terminal projection',
  );
  return { accepted, events };
}

async function verifyPair(baseUrl: string, worker: RuntimeKind, judge: RuntimeKind) {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
  const session = await client.beta.sessions.create({
    agent: worker === 'acp' ? 'acp-agent' : 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
  await sendMessage(client, session.id, 'prepare a draft');
  const { events } = await defineOutcome(
    client,
    session.id,
    { type: 'text', content: 'FINAL' },
    3,
  );
  const ends = outcomeEnds(events);
  assert.deepEqual(ends.map((event) => event.iteration), [0, 1]);
  assert.deepEqual(ends.map((event) => event.result), ['needs_revision', 'satisfied']);
  assert.equal(new Set(ends.map((event) => event.outcome_id)).size, 1);
  assert.ok(
    ends.at(-1)?.explanation.includes(judge === 'acp' ? 'ACP judge' : 'native judge'),
    `${judge} Judge evidence was not projected: ${JSON.stringify(ends)}`,
  );

  const agentText = events
    .filter((event) => event.type === 'agent.message')
    .flatMap((event) => event.content ?? [])
    .map((content) => content.type === 'text' ? content.text : '')
    .join('\n');
  assert.ok(
    agentText.includes(worker === 'acp' ? 'rough draft from ACP worker' : 'FINAL answer'),
    `${worker} Worker marker did not reach the transcript: ${agentText}`,
  );
  pass(`${worker} Worker × ${judge} Judge -> needs_revision(0), satisfied(1)`);
}

async function verifyDefinitionProjection(baseUrl: string) {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

  // P1/P2: omission and explicit null are observably the same retained None;
  // the default of three exists only when the Outcome effect is prepared.
  const defaultCases: Array<[string, undefined | null]> = [['P1', undefined], ['P2', null]];
  for (const [rule, maxIterations] of defaultCases) {
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    const { accepted, events } = await defineOutcome(
      client,
      session.id,
      { type: 'text', content: 'NEVER_PRESENT_TOKEN' },
      maxIterations,
    );
    assert.deepEqual(accepted.rubric, { type: 'text', content: 'NEVER_PRESENT_TOKEN' }, `${rule}/E2`);
    assert.equal(accepted.max_iterations, null, `${rule}/E2 retained max is None`);
    const ends = outcomeEnds(events, accepted.outcome_id);
    assert.deepEqual(ends.map((event) => event.iteration), [0, 1, 2], `${rule}/E3 default applies at effect`);
    assert.equal(ends.at(-1)?.result, 'max_iterations_reached', `${rule}/E3 terminal result`);
  }

  // P3: a real Files API id remains a File rubric in root provenance while an
  // explicit max stays Some(2); execution consumes the reference without
  // rewriting the public definition as Text.
  const rubricFile = await client.beta.files.upload({
    file: await toFile(Buffer.from('file-backed Outcome rubric'), 'outcome-rubric.txt'),
    betas: BETAS,
  });
  const fileSession = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
  const { accepted, events } = await defineOutcome(
    client,
    fileSession.id,
    { type: 'file', file_id: rubricFile.id },
    2,
  );
  assert.deepEqual(accepted.rubric, { type: 'file', file_id: rubricFile.id }, 'P3/E2 exact File rubric');
  assert.equal(accepted.max_iterations, 2, 'P3/E2 explicit Some(2)');
  const fileEnds = outcomeEnds(events, accepted.outcome_id);
  assert.deepEqual(fileEnds.map((event) => event.iteration), [0, 1], 'P3/E4 explicit two-Grade cap');
  assert.equal(fileEnds.at(-1)?.result, 'max_iterations_reached', 'P3/E4 terminal result');
  pass('P1-P3 Outcome Text/File and omitted/null/explicit max projection');
}

async function verifyBudgetOneAcknowledgment(baseUrl: string) {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
  const session = await client.beta.sessions.create({
    agent: 'acp-agent',
    environment_id: 'env_local',
    betas: BETAS,
  });
  await sendMessage(client, session.id, 'prepare a draft');
  const { events } = await defineOutcome(
    client,
    session.id,
    { type: 'text', content: 'NEVER_PRESENT_TOKEN' },
    1,
  );
  const ends = outcomeEnds(events);
  assert.equal(ends.length, 1, 'max_iterations=1 permits exactly one Grade');
  assert.equal(ends[0].iteration, 0);
  assert.equal(ends[0].result, 'max_iterations_reached');
  const text = events
    .filter((event) => event.type === 'agent.message')
    .flatMap((event) => event.content ?? [])
    .map((content) => content.type === 'text' ? content.text : '')
    .join('\n');
  assert.ok(text.includes('acknowledged remaining feedback'), 'ungraded acknowledgment is visible');
  pass('max_iterations=1 -> one Grade and one ungraded acknowledgment');
}

async function verifyDefinitionBoundaries(baseUrl: string) {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
  const invalid = [
    { description: '', rubric: 'FINAL', max_iterations: 1 },
    { description: 'produce the final deliverable', rubric: '', max_iterations: 1 },
    { description: 'produce the final deliverable', rubric: 'FINAL', max_iterations: 0 },
    { description: 'produce the final deliverable', rubric: 'FINAL', max_iterations: 21 },
  ];
  for (const candidate of invalid) {
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    await assert.rejects(
      client.beta.sessions.events.send(session.id, {
        events: [{
          type: 'user.define_outcome',
          description: candidate.description,
          rubric: { type: 'text', content: candidate.rubric },
          max_iterations: candidate.max_iterations,
        }],
        betas: BETAS,
      }),
      (error: unknown) => (error as { status?: number })?.status === 400,
      `invalid Outcome definition must fail at the API boundary: ${JSON.stringify(candidate)}`,
    );
  }
  pass('definition partitions reject blank description/rubric and budgets outside 1..=20');
}

async function runJudge(judge: RuntimeKind, port: number) {
  await withScenarioServer(
    'outcome-matrix',
    'revise',
    port,
    async (baseUrl: string) => {
      await verifyPair(baseUrl, 'native', judge);
      await verifyPair(baseUrl, 'acp', judge);
      if (judge === 'native') {
        await verifyDefinitionProjection(baseUrl);
      }
      if (judge === 'acp') {
        await verifyBudgetOneAcknowledgment(baseUrl);
        await verifyDefinitionBoundaries(baseUrl);
      }
    },
    { AWAKEN_OUTCOME_JUDGE_RUNTIME: judge },
  );
}

async function main() {
  await runJudge('native', 38434);
  await runJudge('acp', 38435);
  console.log('E2E PASS: Managed Outcome Native/ACP Worker×Judge matrix + budget boundary.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
