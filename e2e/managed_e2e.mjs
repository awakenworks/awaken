// Comprehensive Managed Agents e2e with the official Anthropic TypeScript SDK.
// The model runs for real: `withRealServer('echo', …)` boots the server in
// `AWAKEN_MODEL_MODE=real`, so every turn goes through GenaiExecutor + a real
// socket to a fake Anthropic upstream reproducing the echo scenario on the wire —
// no in-process stub model. Covers: session create + retrieve, single and
// multi-turn messages, event list, SSE stream (events.stream), and error handling.
//
// Run: (from e2e/)  npm install && node managed_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass, waitForSessionEventReceipt } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38099);
const BETAS = ['managed-agents-2026-04-01'];

async function sendMessage(client, sessionId, text) {
  return client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

async function main() {
  await withRealServer('echo', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    // --- create + retrieve ---
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    assert.equal(session.type, 'session');
    assert.equal(session.status, 'idle');
    assert.ok(session.id.startsWith('sesn_'));

    const retrieved = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.equal(retrieved.id, session.id);
    assert.equal(retrieved.agent.id, 'assistant');
    pass('create + retrieve');

    // --- single message: the reply came back through the real provider wire ---
    // Receipt observation graph: C1=the SDK returns an exact durable User
    // receipt; C2=the Run later commits its complete ordered wire sequence.
    // Effects: E1=the receipt owns the opening User Event; E2=the primary
    // Thread bracket and model span close before usage and aggregate idle.
    // Constraint: the Session event ledger is authoritative and this fresh
    // Session has no older history. Decision rules: M1 C1&&!C2=>observe;
    // M2 C1+C2=>assert the exact full-history sequence E1+E2.
    const firstReceipt = await sendMessage(client, session.id, 'hi there');
    const firstReceiptId = firstReceipt.data[0]?.id;
    assert.equal(typeof firstReceiptId, 'string', 'M1 exact first User Event receipt');
    let { events } = await waitForSessionEventReceipt(
      client,
      session.id,
      firstReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'session.usage')
        && delta.some((event) => event.type === 'session.status_idle'),
      'M1 first Run to commit its complete wire sequence',
    );
    assert.deepEqual(events.map((e) => e.type), [
      'user.message',
      'session.status_running',
      'session.thread_status_running',
      'span.model_request_start',
      'span.model_request_end',
      'agent.message',
      'session.thread_status_idle',
      'session.usage',
      'session.status_idle',
    ]);
    assert.equal(events.find((e) => e.type === 'agent.message').content[0].text, 'Echo: hi there');
    assert.equal(events.find((e) => e.type === 'session.status_idle').stop_reason.type, 'end_turn');
    pass('single message + list');

    // --- multi-turn conversation ---
    const secondReceipt = await sendMessage(client, session.id, 'second');
    const secondReceiptId = secondReceipt.data[0]?.id;
    assert.equal(typeof secondReceiptId, 'string', 'M2 exact second User Event receipt');
    const second = await waitForSessionEventReceipt(
      client,
      session.id,
      secondReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.message')
        && delta.some((event) => event.type === 'session.status_idle')
        && delta.some((event) => event.type === 'session.usage'),
      'M2 second Run to commit its complete wire sequence',
    );
    events = second.events;
    const messages = events.filter((e) => e.type === 'agent.message').map((e) => e.content[0].text);
    assert.deepEqual(messages, ['Echo: hi there', 'Echo: second']);
    pass('multi-turn conversation');

    // --- SSE stream (events.stream) ---
    const stream = await client.beta.sessions.events.stream(session.id, { betas: BETAS });
    const streamedTypes = [];
    for await (const ev of stream) streamedTypes.push(ev.type);
    assert.ok(streamedTypes.includes('agent.message'), `stream types: ${streamedTypes}`);
    assert.ok(streamedTypes.includes('session.status_idle'), `stream types: ${streamedTypes}`);
    pass('SSE stream via events.stream');

    // --- errors: unknown session (status + Anthropic error envelope) ---
    await assert.rejects(
      () => client.beta.sessions.retrieve('sesn_does_not_exist', { betas: BETAS }),
      (err) => {
        assert.equal(err.status, 404);
        // `err.error` is the parsed body: { type: 'error', error: { type, message } }.
        assert.equal(err.error?.type, 'error');
        assert.equal(err.error?.error?.type, 'not_found_error');
        assert.ok(err.error?.error?.message, 'error message is populated');
        return true;
      },
    );
    pass('unknown session -> 404 + not_found_error envelope');

    // --- errors: a malformed body is rejected in the same envelope shape ---
    const bad = await fetch(`${baseUrl}/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
      body: '{ not valid json',
    });
    assert.equal(bad.status, 400);
    const badBody = await bad.json();
    assert.equal(badBody.type, 'error');
    assert.equal(badBody.error.type, 'invalid_request_error');
    assert.ok(badBody.error.message, 'decode-failure message is populated');

    // --- the managed-agents beta must be opted into to create a session ---
    const noBeta = await fetch(`${baseUrl}/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ agent: 'assistant', environment_id: 'env_local' }),
    });
    assert.equal(noBeta.status, 400, 'create without the beta header is a 400');
    const noBetaBody = await noBeta.json();
    assert.equal(noBetaBody.error.type, 'invalid_request_error');
    assert.match(noBetaBody.error.message, /managed-agents-2026-04-01/, 'the error names the required beta');
    pass('session create requires the anthropic-beta: managed-agents-2026-04-01 header');
    pass('malformed body -> 400 + invalid_request_error envelope');

    console.log('E2E PASS: Managed Agents lifecycle/messages/stream/errors via TS SDK (real provider wire).');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
