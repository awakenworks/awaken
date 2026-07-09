// session.error e2e: a turn whose run ends in a terminal fault surfaces the
// failure as a committed `session.error` event (the SDK models it as a stream
// event, not an HTTP error — the /events POST is still accepted). The `error`
// scenario model fails any turn containing `BOOM` with a permanent provider
// error; other turns echo, so we also prove the session stays usable afterward.
//
// Run: (from e2e/)  node managed_session_error_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38251);
const BETAS = ['managed-agents-2026-04-01'];

async function allEvents(client, id) {
  const evs = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) evs.push(ev);
  return evs;
}

async function send(client, id, text) {
  await client.beta.sessions.events.send(id, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
}

async function main() {
  const { server, baseUrl } = spawnServer('error', PORT);
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const s = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });

    // A normal turn echoes — the session is healthy before the failure.
    await send(client, s.id, 'hello');
    let types = (await allEvents(client, s.id)).map((e) => e.type);
    assert.ok(types.includes('agent.message'), `healthy turn replied — ${types.join(',')}`);
    pass('a normal turn echoes (session healthy)');

    // The failing turn is accepted (the events POST returns receipts); the failure
    // surfaces as a committed event, not an HTTP error.
    await send(client, s.id, 'please BOOM now');
    pass('failing turn is accepted (events POST returns 200)');

    // A session.error event is committed so a listing/streaming client sees it.
    const events = await allEvents(client, s.id);
    const errEv = events.find((e) => e.type === 'session.error');
    assert.ok(errEv, `session.error committed to the log — ${events.map((e) => e.type).join(',')}`);
    assert.equal(typeof errEv.id, 'string', 'session.error has an id');
    assert.equal(typeof errEv.processed_at, 'string', 'session.error has processed_at');
    assert.equal(errEv.error.type, 'unknown_error', 'error object is the unknown_error fallback');
    assert.equal(typeof errEv.error.message, 'string', 'error carries a human-readable message');
    assert.equal(errEv.error.retry_status.type, 'exhausted', 'retry_status is exhausted');
    pass('session.error event committed with the SDK shape (unknown_error + exhausted)');

    // The session survives a failed turn: a subsequent normal turn still echoes.
    await send(client, s.id, 'still there?');
    types = (await allEvents(client, s.id)).map((e) => e.type);
    const echoes = (await allEvents(client, s.id)).filter((e) => e.type === 'agent.message');
    assert.ok(echoes.length >= 2, `session usable after the error (${echoes.length} agent.message)`);
    pass('the session stays usable after a failed turn');

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
