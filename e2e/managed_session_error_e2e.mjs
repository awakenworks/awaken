// session.error e2e: a provider fault without terminal execution evidence
// surfaces as a committed terminal-unknown `session.error`. The accepted input
// and physical Dispatch slot remain occupied; a sibling Session stays usable.
//
// Run: (from e2e/)  node managed_session_error_e2e.mjs

import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree,
  pass,
  spawnServer,
  stopServer,
  waitForPort,
  waitForRetainedUnknownExecution,
  waitForSessionEventReceipt,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38251);
const BETAS = ['managed-agents-2026-04-01'];
const ROOT = path.join(os.tmpdir(), `awaken-managed-session-error-${process.pid}`);
const DISPATCH_DATABASE = path.join(ROOT, 'store', 'dispatch.db');
const SERVER_ENV = {
  SESSION_DEPLOYMENT_STORAGE_DIR: path.join(ROOT, 'store'),
  SESSION_DEPLOYMENT_SANDBOX_DIR: path.join(ROOT, 'sandboxes'),
};

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
  // C2=BOOM triggers a provider fault after request entry without a terminal
  // receipt; C3=a later normal Run targets a sibling Session. Effects: E1=a
  // healthy input commits; E2=the failed POST stays accepted, input remains
  // unprocessed, terminal unknown is public, and the exact slot stays occupied;
  // E3=the sibling Session remains usable. Decision rules: R1 C0+C1=>E1;
  // R2 C0+C2=>E2; R3 R2+C3=>E3. The shared retained-unknown oracle is also
  // consumed by the Native/ACP runtime matrix, so both suites use one authority.
  cleanupFixtureTree(ROOT);
  const { server, baseUrl } = spawnServer('error', PORT, SERVER_ENV);
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
    const [failedReceipt] = (await client.beta.sessions.events.send(s.id, {
      betas: BETAS,
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'please BOOM now' }] }],
    })).data;
    const retained = await waitForRetainedUnknownExecution(
      client,
      s.id,
      failedReceipt.id,
      BETAS,
      DISPATCH_DATABASE,
      'provider fault to retain terminal-unknown execution',
    );
    events = retained.events;
    pass('failing Run is accepted (events POST returns 200)');

    // A session.error event is committed so a listing/streaming client sees it.
    const errEv = events.find((e) => e.type === 'session.error');
    assert.ok(errEv, `session.error committed to the log — ${events.map((e) => e.type).join(',')}`);
    assert.equal(typeof errEv.id, 'string', 'session.error has an id');
    assert.equal(typeof errEv.processed_at, 'string', 'session.error has processed_at');
    assert.equal(errEv.error.type, 'unknown_error', 'error object is the unknown_error fallback');
    assert.equal(typeof errEv.error.message, 'string', 'error carries a human-readable message');
    assert.equal(errEv.error.retry_status.type, 'terminal', 'retry_status is terminal');
    assert.equal(retained.session.status, 'running', 'aggregate occupancy remains visible');
    pass('session.error commits terminal unknown while the exact input/slot remain occupied');

    // The occupied Session rejects overlap by remaining running. A sibling
    // Session proves the service and model route remain healthy.
    const sibling = await client.beta.sessions.create({
      agent: 'assistant', environment_id: 'env_local', betas: BETAS,
    });
    events = await sendAndWait(
      client,
      sibling.id,
      'still there?',
      (delta) => delta.some((event) => event.type === 'agent.message'),
    );
    types = events.map((e) => e.type);
    const echoes = events.filter((e) => e.type === 'agent.message');
    assert.equal(echoes.length, 1, `sibling Session usable after the error (${echoes.length} agent.message)`);
    pass('a sibling Session stays usable while the unknown slot remains fenced');

    console.log('E2E PASS: provider uncertainty is public, fenced, and isolated to its Session.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server);
    cleanupFixtureTree(ROOT);
  }
}

main();
