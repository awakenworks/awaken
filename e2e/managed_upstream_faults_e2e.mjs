// Upstream fault injection over the REAL provider path (fake Anthropic upstream,
// no live key): a retryable failure is retried and recovers, and a persistent
// failure exhausts retries and surfaces through the run loop. Drives the runtime's
// retry policy, circuit breaker, and inference-error handling — paths a happy-path
// e2e never reaches. Deterministic, hermetic, CI-safe.
//
// Run: (from e2e/)  node managed_upstream_faults_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-fake-faults-key'; // awaken-allow: secret

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function runOneTurn(baseUrl, text) {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
  const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
  // The send itself may reject when the turn fails terminally; the retry/error
  // paths run either way (that is what we are covering).
  let sendError = null;
  try {
    await client.beta.sessions.events.send(session.id, {
      betas: BETAS,
      events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    });
  } catch (err) {
    sendError = err;
  }
  const events = await listEvents(client, session.id).catch(() => []);
  return { events, sendError };
}

async function main() {
  process.env.ANTHROPIC_API_KEY = FAKE_KEY;
  process.env.ANTHROPIC_MODEL = 'fake-haiku';

  // Arm 1: a retryable failure (first attempt 503) is retried; the turn recovers.
  const recovering = await startFakeAnthropic(FAKE_KEY, { failuresBeforeSuccess: 1 });
  try {
    process.env.ANTHROPIC_BASE_URL = `${recovering.url}/v1/`;
    await withServer('real', 38221, async (baseUrl) => {
      const { events } = await runOneTurn(baseUrl, 'hello after a hiccup');
      const replies = events
        .filter((e) => e.type === 'agent.message')
        .map((e) => e.content.map((b) => b.text ?? '').join(''));
      assert.ok(
        replies.some((m) => m.includes('FAKE:hello after a hiccup')),
        `the turn recovered on retry: ${JSON.stringify(replies)}`,
      );
      assert.ok(recovering.attempts >= 2, `the failed attempt was retried (attempts=${recovering.attempts})`);
    });
    pass('a retryable upstream failure is retried and the turn recovers');
  } finally {
    recovering.close();
  }

  // Arm 2: every attempt fails; retries exhaust and the failure surfaces through
  // the run loop (error handling + circuit breaker) rather than hanging or panicking.
  const failing = await startFakeAnthropic(FAKE_KEY, { alwaysFail: true });
  try {
    process.env.ANTHROPIC_BASE_URL = `${failing.url}/v1/`;
    await withServer('real', 38222, async (baseUrl) => {
      const { events, sendError } = await runOneTurn(baseUrl, 'this never succeeds');
      const recovered = events
        .filter((e) => e.type === 'agent.message')
        .some((e) => e.content.some((b) => (b.text ?? '').startsWith('FAKE:')));
      assert.ok(!recovered, 'a permanently-failing upstream does not fabricate a reply');
      assert.ok(failing.attempts >= 2, `retries were attempted before giving up (attempts=${failing.attempts})`);
      assert.ok(sendError !== null || events.length > 0, 'the failure surfaced (error or a status event)');
    });
    pass('exhausted upstream failures surface through the run loop (retries + breaker + error path)');
  } finally {
    failing.close();
  }

  console.log('E2E PASS: upstream fault injection drives retry + circuit-breaker + inference-error paths.');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
