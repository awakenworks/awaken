// session.error e2e: a Run that ends in a terminal fault surfaces the
// failure as a committed `session.error` event (the SDK models it as a stream
// event, not an HTTP error — the /events POST is still accepted). The `error`
// scenario model fails any Run containing `BOOM` with a permanent provider
// error; other Runs echo, so we also prove the session stays usable afterward.
//
// Run: (from e2e/)  node managed_session_error_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  pass,
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38251);
const BETAS = ['managed-agents-2026-04-01'];

async function sendAndWait(client, id, text, terminalEffect) {
  const receipt = await client.beta.sessions.events.send(id, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  const acceptedId = receipt.data[0]?.id;
  const { events } = await waitForSessionEventReceipt(
    client,
    id,
    acceptedId,
    BETAS,
    ({ delta }) => terminalEffect(delta),
    `the Run for ${text} to commit its terminal public effect`,
  );
  return events;
}

async function main() {
  // Cause/effect graph: C0=the POST first returns a durable Event receipt;
  // C1=a normal Run reaches the healthy model path;
  // C2=BOOM triggers a permanent provider fault after event admission; C3=a
  // later normal Run targets the same Session. Effects: E1=agent.message commits;
  // E2=POST stays accepted while session.error commits with unknown_error and
  // exhausted retry status; E3=the Session remains usable. Decision rules:
  // R1 C0 && C1 => E1; R2 C0 && C2 => E2; R3 R2 && C3 => E3.
  // Constraints/invariant: a permanent Run fault is a committed public Event,
  // not an HTTP rollback, and it does not terminate the reusable Session.
  const { server, baseUrl } = spawnServer('error', PORT);
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

    // A normal Run echoes — the session is healthy before the failure.
    let events = await sendAndWait(
      client,
      s.id,
      'hello',
      (delta) => delta.some((event) => event.type === 'agent.message'),
    );
    let types = events.map((e) => e.type);
    assert.ok(types.includes('agent.message'), `healthy Run replied — ${types.join(',')}`);
    pass('a normal Run echoes (session healthy)');

    // The failing Run is accepted (the events POST returns receipts); the failure
    // surfaces as a committed event, not an HTTP error.
    events = await sendAndWait(
      client,
      s.id,
      'please BOOM now',
      (delta) => delta.some((event) => event.type === 'session.error'),
    );
    pass('failing Run is accepted (events POST returns 200)');

    // A session.error event is committed so a listing/streaming client sees it.
    const errEv = events.find((e) => e.type === 'session.error');
    assert.ok(errEv, `session.error committed to the log — ${events.map((e) => e.type).join(',')}`);
    assert.equal(typeof errEv.id, 'string', 'session.error has an id');
    assert.equal(typeof errEv.processed_at, 'string', 'session.error has processed_at');
    assert.equal(errEv.error.type, 'unknown_error', 'error object is the unknown_error fallback');
    assert.equal(typeof errEv.error.message, 'string', 'error carries a human-readable message');
    assert.equal(errEv.error.retry_status.type, 'exhausted', 'retry_status is exhausted');
    pass('session.error event committed with the SDK shape (unknown_error + exhausted)');

    // The Session survives a failed Run: a subsequent normal Run still echoes.
    events = await sendAndWait(
      client,
      s.id,
      'still there?',
      (delta) => delta.some((event) => event.type === 'agent.message'),
    );
    types = events.map((e) => e.type);
    const echoes = events.filter((e) => e.type === 'agent.message');
    assert.ok(echoes.length >= 2, `session usable after the error (${echoes.length} agent.message)`);
    pass('the Session stays usable after a failed Run');

    console.log('E2E PASS: a terminal run fault commits session.error to the event log; the session survives.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server);
  }
}

main();
