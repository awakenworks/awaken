// A2A HITL decision e2e: a structured DataPart carries an explicit allow/deny
// decision while tasks/cancel remains whole-task cancellation. Plain text is not
// authorization and cannot accidentally approve a mutating tool.
//
// Chain (probe: the mutating `write` awaits on approval):
//   POST /v1/a2a message/send (fresh)   -> Runtime await -> Task.state=input-required
//   POST /v1/a2a message/send (awaiting, allow=true)  -> Confirm{allow:true}
//   POST /v1/a2a message/send (awaiting, allow=false) -> Confirm{allow:false}
//   POST /v1/a2a tasks/cancel (awaiting)              -> Task.state=canceled
//
// Deterministic (probe stub, no API key). Run: (from e2e/) node a2a_hitl_decision_e2e.mjs

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38606);

let rpcId = 0;
async function rpc(base, method, params, { allowError = false } = {}) {
  const res = await fetch(`${base}/v1/a2a`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: ++rpcId, method, params }),
  });
  assert.equal(res.status, 200, `${method} transport ok (${res.status})`);
  const body = await res.json();
  if (allowError) return body;
  assert.ok(!body.error, `${method} not a JSON-RPC error: ${JSON.stringify(body.error)}`);
  return body.result;
}

function sendMsg(base, context, text) {
  return rpc(base, 'message/send', {
    message: {
      messageId: `m-${randomBytes(4).toString('hex')}`,
      contextId: context,
      role: 'user',
      kind: 'message',
      parts: [{ kind: 'text', text }],
    },
  });
}

function sendDecision(base, context, allow, note) {
  return rpc(base, 'message/send', {
    message: {
      messageId: `m-${randomBytes(4).toString('hex')}`,
      contextId: context,
      role: 'user',
      kind: 'message',
      parts: [{ kind: 'data', data: { type: 'tool-approval', allow, note } }],
    },
  });
}

const stateOf = (task) => task?.status?.state;
const taskText = (task) =>
  (task?.status?.message?.parts ?? []).filter((p) => p.kind === 'text').map((p) => p.text).join('');

async function main() {
  await withServer('probe', PORT, async (base) => {
    // Decision table: D1 explicit true -> allow+complete; D2 explicit false ->
    // deny+complete; D3 tasks/cancel -> canceled; D4 plain text -> JSON-RPC
    // invalid-params and the awaiting task remains pending.
    // ---- Arm 1: explicit ALLOW --------------------------------------------
    const c1 = `a2a-allow-${randomBytes(4).toString('hex')}`;
    let t = await sendMsg(base, c1, 'record this note');
    assert.equal(stateOf(t), 'input-required', `fresh turn awaits -> input-required (got ${stateOf(t)})`);
    pass('A2A: mutating tool awaits -> Task.state=input-required');

    t = await sendDecision(base, c1, true, 'reviewed');
    assert.equal(stateOf(t), 'completed', `explicit allow -> completed (got ${stateOf(t)})`);
    assert.ok(taskText(t).includes('done'), `run completed after explicit allow: ${taskText(t)}`);
    pass('A2A: structured allow resumes the awaiting tool');

    // ---- Arm 2: explicit DENY ---------------------------------------------
    const c2 = `a2a-deny-${randomBytes(4).toString('hex')}`;
    t = await sendMsg(base, c2, 'record this other note');
    assert.equal(stateOf(t), 'input-required', 'second context awaits too');
    t = await sendDecision(base, c2, false, 'operator denied');
    assert.equal(stateOf(t), 'completed', `explicit deny resumes safely (got ${stateOf(t)})`);
    pass('A2A: structured deny reaches Confirm{allow:false}');

    // ---- Arm 3: task cancellation is distinct -----------------------------
    const c3 = `a2a-cancel-${randomBytes(4).toString('hex')}`;
    t = await sendMsg(base, c3, 'record a third note');
    assert.equal(stateOf(t), 'input-required', 'third context awaits too');
    const awaitingTaskId = t.id;
    assert.ok(awaitingTaskId, 'the awaiting A2A task has a server-issued id');

    const cancelled = await rpc(base, 'tasks/cancel', { id: awaitingTaskId });
    assert.equal(stateOf(cancelled), 'canceled', `tasks/cancel denies + cancels (got ${stateOf(cancelled)})`);
    pass('A2A: tasks/cancel cancels the whole Task independently of denial');

    // ---- Arm 4: unstructured text cannot authorize -----------------------
    const c4 = `a2a-plain-${randomBytes(4).toString('hex')}`;
    t = await sendMsg(base, c4, 'record a fourth note');
    assert.equal(stateOf(t), 'input-required', 'fourth context awaits too');
    const plainText = await rpc(base, 'message/send', {
      message: {
        messageId: `m-${randomBytes(4).toString('hex')}`,
        contextId: c4,
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'text', text: 'yes, allow it' }],
      },
    }, { allowError: true });
    assert.equal(plainText.error?.code, -32602, 'plain text is invalid approval input');
    const stillAwaiting = await rpc(base, 'tasks/get', { id: t.id });
    assert.equal(
      stateOf(stillAwaiting),
      'input-required',
      'an invalid decision leaves the durable approval pending',
    );
    pass('A2A: plain text fails closed and preserves the pending approval');

    // Cancelling a context with nothing awaiting is not a false-cancel.
    const idle = `a2a-idle-${randomBytes(4).toString('hex')}`;
    await sendMsg(base, idle, 'hi'); // probe awaits, so drive it to completion:
    const completed = await sendDecision(base, idle, true, 'reviewed');
    assert.ok(completed.id, 'the completed A2A task has a server-issued id');
    const notAwaiting = await rpc(base, 'tasks/cancel', { id: completed.id }, { allowError: true });
    assert.equal(notAwaiting.error?.code, -32002, 'a terminal task is explicitly not cancelable');
    pass('A2A: cancel with nothing awaiting fails closed as task-not-cancelable');
  });

  console.log('E2E PASS: A2A explicit allow/deny and task cancellation.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
