// Live-inbox (live control) over an in-flight turn: a delayed fake upstream keeps
// a turn running while the client snapshots + queues + reorders + replaces +
// removes queued messages, and exercises the error arms (queue on idle -> 410,
// bad permutation, unknown message id). Drives the managed router's five
// live-inbox handlers + the host's live-inbox edit path. Deterministic, CI-safe.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = 'managed-agents-2026-04-01';
const FAKE_KEY = 'sk-fake-liveinbox'; // awaken-allow: secret
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

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
  const upstream = await startFakeAnthropic(FAKE_KEY, { delayMs: 4000 });
  try {
    process.env.ANTHROPIC_API_KEY = FAKE_KEY;
    process.env.ANTHROPIC_BASE_URL = `${upstream.url}/v1/`;
    process.env.ANTHROPIC_MODEL = 'fake-haiku';
    await withServer('real', 38271, async (base) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
      const session = await client.beta.sessions.create({ agent: 'assistant', betas: [BETAS] });
      const id = session.id;

      // Idle: snapshot is empty; queueing is refused (no turn in flight).
      let r = await li(base, 'GET', `/v1/sessions/${id}/live-inbox`);
      assert.equal(r.status, 200);
      assert.equal(r.json.active, false, 'idle inbox is inactive');
      r = await li(base, 'POST', `/v1/sessions/${id}/live-inbox`, { content: [{ type: 'text', text: 'x' }] });
      assert.equal(r.status, 410, `queue on idle -> 410 (got ${r.status})`);
      pass('idle live-inbox: empty snapshot + queue refused (410)');

      // Start a slow turn (the fake holds the inference call ~4s); do not await yet.
      const turn = client.beta.sessions.events.send(id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'run slowly' }] }],
        betas: [BETAS],
      });
      await sleep(700); // let the turn reach the (delayed) inference call

      r = await li(base, 'GET', `/v1/sessions/${id}/live-inbox`);
      assert.equal(r.json.active, true, 'the inbox is active during an in-flight turn');

      const q1 = await li(base, 'POST', `/v1/sessions/${id}/live-inbox`, { content: [{ type: 'text', text: 'msg one' }] });
      assert.equal(q1.status, 200, `queue m1: ${JSON.stringify(q1.json)}`);
      const q2 = await li(base, 'POST', `/v1/sessions/${id}/live-inbox`, { content: [{ type: 'text', text: 'msg two' }] });
      assert.equal(q2.status, 200);
      r = await li(base, 'GET', `/v1/sessions/${id}/live-inbox`);
      assert.equal(r.json.messages.length, 2, 'two messages queued');

      // Reorder (full permutation), replace one, remove the other.
      r = await li(base, 'PUT', `/v1/sessions/${id}/live-inbox/order`, { order: [q2.json.id, q1.json.id] });
      assert.equal(r.status, 204, `reorder -> 204 (got ${r.status})`);
      r = await li(base, 'PUT', `/v1/sessions/${id}/live-inbox/${q1.json.id}`, { content: [{ type: 'text', text: 'edited' }] });
      assert.equal(r.status, 204, `replace -> 204 (got ${r.status})`);
      r = await li(base, 'DELETE', `/v1/sessions/${id}/live-inbox/${q2.json.id}`);
      assert.equal(r.status, 204, `remove -> 204 (got ${r.status})`);
      pass('live-inbox queue/reorder/replace/remove during an in-flight turn');

      // Error arms: a bad permutation and an unknown message id.
      r = await li(base, 'PUT', `/v1/sessions/${id}/live-inbox/order`, { order: [99999] });
      assert.ok(r.status >= 400, `bad permutation -> 4xx (got ${r.status})`);
      r = await li(base, 'PUT', `/v1/sessions/${id}/live-inbox/99999`, { content: [{ type: 'text', text: 'x' }] });
      assert.ok(r.status >= 400, `replace unknown id -> 4xx (got ${r.status})`);
      pass('live-inbox edit error arms: bad permutation + unknown id -> 4xx');

      await turn.catch(() => {}); // let the turn drain
    });
    console.log('E2E PASS: live-inbox snapshot/queue/reorder/replace/remove + error arms over an in-flight turn.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    upstream.close();
  }
}

main();
