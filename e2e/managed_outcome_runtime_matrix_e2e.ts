// Managed Outcome end-to-end backend matrix through the official Anthropic TS
// SDK. Every Worker/Judge Native×ACP pairing must use the same Run interface and
// produce the same zero-based lifecycle.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function listEvents(client, sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return events;
}

async function sendMessage(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

async function defineOutcome(client, sessionId, rubric, maxIterations) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{
      type: 'user.define_outcome',
      description: 'produce the final deliverable',
      rubric: { type: 'text', content: rubric },
      max_iterations: maxIterations,
    }],
    betas: BETAS,
  });
}

async function verifyPair(baseUrl, worker, judge) {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    ...(worker === 'acp' ? { metadata: { 'awaken.runtime': 'acp:claude' } } : {}),
    betas: BETAS,
  });
  await sendMessage(client, session.id, 'prepare a draft');
  await defineOutcome(client, session.id, 'FINAL', 3);

  const events = await listEvents(client, session.id);
  const ends = events.filter((event) => event.type === 'span.outcome_evaluation_end');
  assert.deepEqual(ends.map((event) => event.iteration), [0, 1]);
  assert.deepEqual(ends.map((event) => event.result), ['needs_revision', 'satisfied']);
  assert.equal(new Set(ends.map((event) => event.outcome_id)).size, 1);
  assert.ok(
    ends.at(-1).explanation.includes(judge === 'acp' ? 'ACP judge' : 'native judge'),
    `${judge} Judge evidence was not projected: ${JSON.stringify(ends)}`,
  );

  const agentText = events
    .filter((event) => event.type === 'agent.message')
    .flatMap((event) => event.content ?? [])
    .map((content) => content.text ?? '')
    .join('\n');
  assert.ok(
    agentText.includes(worker === 'acp' ? 'rough draft from ACP worker' : 'FINAL answer'),
    `${worker} Worker marker did not reach the transcript: ${agentText}`,
  );
  pass(`${worker} Worker × ${judge} Judge -> needs_revision(0), satisfied(1)`);
}

async function verifyBudgetOneAcknowledgment(baseUrl) {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    metadata: { 'awaken.runtime': 'acp:claude' },
    betas: BETAS,
  });
  await sendMessage(client, session.id, 'prepare a draft');
  await defineOutcome(client, session.id, 'NEVER_PRESENT_TOKEN', 1);

  const events = await listEvents(client, session.id);
  const ends = events.filter((event) => event.type === 'span.outcome_evaluation_end');
  assert.equal(ends.length, 1, 'max_iterations=1 permits exactly one Grade');
  assert.equal(ends[0].iteration, 0);
  assert.equal(ends[0].result, 'max_iterations_reached');
  const text = events
    .filter((event) => event.type === 'agent.message')
    .flatMap((event) => event.content ?? [])
    .map((content) => content.text ?? '')
    .join('\n');
  assert.ok(text.includes('acknowledged remaining feedback'), 'ungraded acknowledgment is visible');
  pass('max_iterations=1 -> one Grade and one ungraded acknowledgment');
}

async function verifyDefinitionBoundaries(baseUrl) {
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
      (error) => error?.status === 400,
      `invalid Outcome definition must fail at the API boundary: ${JSON.stringify(candidate)}`,
    );
  }
  pass('definition partitions reject blank description/rubric and budgets outside 1..=20');
}

async function runJudge(judge, port) {
  await withScenarioServer(
    'outcome-matrix',
    'revise',
    port,
    async (baseUrl) => {
      await verifyPair(baseUrl, 'native', judge);
      await verifyPair(baseUrl, 'acp', judge);
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
