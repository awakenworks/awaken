// Live-inbox (live control) over an in-flight turn: a delayed fake upstream keeps
// a turn running while the client snapshots + queues + reorders + replaces +
// removes queued messages, and exercises the error arms (queue on idle -> 410,
// bad permutation, unknown message id). Drives the managed router's five
// live-inbox handlers + the host's live-inbox edit path. Deterministic, CI-safe.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  withRealServer,
  pass,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';

const BETAS = 'managed-agents-2026-04-01';
async function li(base, method, uri, body) {
  const res = await fetch(`${base}${uri}`, {
    method,
    headers: { 'anthropic-beta': BETAS, ...(body ? { 'content-type': 'application/json' } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  return { status: res.status, json: text ? JSON.parse(text) : null };
}

async function main() {
  try {
    await withRealServer('default', 38271, async (base) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: [BETAS],
      });
      const id = session.id;
      const inbox = `/v1/awaken/sessions/${id}/live-inbox`;

      // Cause-effect graph / decision table:
      // R1 turn=idle, operation=snapshot -> 200, inactive, empty queue.
      // R2 turn=idle, operation=queue -> 410 and no queued side effect.
      // These rules prove the namespaced Awaken extension is observable but
      // refuses mutation when no runtime-owned live queue exists.
      let r = await li(base, 'GET', inbox);
      assert.equal(r.status, 200);
      assert.equal(r.json.active, false, 'idle inbox is inactive');
      r = await li(base, 'POST', inbox, { content: [{ type: 'text', text: 'x' }] });
      assert.equal(r.status, 410, `queue on idle -> 410 (got ${r.status})`);
      pass('idle live-inbox: empty snapshot + queue refused (410)');

      // Start a slow turn (the fake holds the inference call ~4s); do not await yet.
      const turn = client.beta.sessions.events.send(id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'run slowly' }] }],
        betas: [BETAS],
      });
      // R3 C1=a non-durable foreground Session Event is dispatched to the local
      // pool Worker; C2=that exact attempt registers its inbox. E1=the Session
      // extension discovers the active inbox. Constraint: deployment.durable is
      // not attempt-ownership evidence; only Runtime's fenced registry is. The
      // decision C1+C2=>E1 is observed without driving either the Worker or server.
      await waitForValue(
        () => li(base, 'GET', inbox),
        (snapshot) => snapshot.status === 200 && snapshot.json.active === true,
        'R3 slow turn exposes its active live inbox',
        { timeoutMs: 5_000, pollMs: 25 },
      );

      // R3 turn=in-flight, operation={snapshot,queue,reorder,replace,remove},
      // ids/order=valid -> success and each ordered mutation is observable.
      r = await li(base, 'GET', inbox);
      assert.equal(r.json.active, true, 'the inbox is active during an in-flight turn');

      const q1 = await li(base, 'POST', inbox, { content: [{ type: 'text', text: 'msg one' }] });
      assert.equal(q1.status, 200, `queue m1: ${JSON.stringify(q1.json)}`);
      const q2 = await li(base, 'POST', inbox, { content: [{ type: 'text', text: 'msg two' }] });
      assert.equal(q2.status, 200);
      r = await li(base, 'GET', inbox);
      assert.equal(r.json.messages.length, 2, 'two messages queued');

      // Reorder (full permutation), replace one, remove the other.
      r = await li(base, 'PUT', `${inbox}/order`, { order: [q2.json.id, q1.json.id] });
      assert.equal(r.status, 204, `reorder -> 204 (got ${r.status})`);
      r = await li(base, 'PUT', `${inbox}/${q1.json.id}`, { content: [{ type: 'text', text: 'edited' }] });
      assert.equal(r.status, 204, `replace -> 204 (got ${r.status})`);
      r = await li(base, 'DELETE', `${inbox}/${q2.json.id}`);
      assert.equal(r.status, 204, `remove -> 204 (got ${r.status})`);
      pass('live-inbox queue/reorder/replace/remove during an in-flight turn');

      // R4 turn=in-flight, operation=reorder, order=not a full permutation -> 4xx.
      // R5 turn=in-flight, operation=replace, id=unknown -> 4xx and no mutation.
      r = await li(base, 'PUT', `${inbox}/order`, { order: [99999] });
      assert.ok(r.status >= 400, `bad permutation -> 4xx (got ${r.status})`);
      r = await li(base, 'PUT', `${inbox}/99999`, { content: [{ type: 'text', text: 'x' }] });
      assert.ok(r.status >= 400, `replace unknown id -> 4xx (got ${r.status})`);
      pass('live-inbox edit error arms: bad permutation + unknown id -> 4xx');

      // R6 C1=in-flight receipt eventually returns; C2=its idle commits after
      // all live edits. E1=the edited turn drains; E2=the settled Worker guard
      // removes the registry entry and the inbox is inactive. Constraint: inbox
      // operations are the only writes; receipt observation is read-only.
      // C1&&!C2=>observe; C1+C2=>E1+E2.
      const receipt = await turn;
      await waitForSessionEventReceipt(
        client,
        id,
        receipt.data[0]?.id,
        [BETAS],
        ({ delta }) => delta.some((event) => event.type === 'session.status_idle'),
        'R6 edited live-inbox turn drains after its exact receipt',
      );
      r = await li(base, 'GET', inbox);
      assert.equal(r.status, 200, 'settled inbox snapshot remains available');
      assert.equal(r.json.active, false, 'settled pool-owned inbox is inactive');
    }, { upstream: { delayMs: 4000 } });
    console.log('E2E PASS: live-inbox snapshot/queue/reorder/replace/remove + error arms over an in-flight turn.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
