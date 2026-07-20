// A2A HITL asymmetry e2e (scenario #5): A2A has NO in-band "deny" for a built-in
// tool approval. `message/send` on an awaiting run reads ANY text as an ALLOW; the only
// protocol-native rejection is `tasks/cancel`, which denies the awaiting tool and
// leaves the task `canceled`. This pins the documented HITL-matrix asymmetry that
// distinguishes A2A from AI-SDK/AG-UI/Managed.
//
// Chain (probe: the mutating `write` awaits on approval):
//   POST /v1/a2a message/send (fresh)   -> Runtime await -> Task.state=input-required
//   POST /v1/a2a message/send (awaiting)  -> to_resume(built-in)=Confirm{allow:true}
//                                          -> run completes -> Task.state=completed  (text = ALLOW)
//   POST /v1/a2a tasks/cancel (awaiting)  -> Confirm{allow:false} -> Task.state=canceled  (the deny path)
//
// Deterministic (probe stub, no API key). Run: (from e2e/) node a2a_hitl_asymmetry_e2e.mjs

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

const stateOf = (task) => task?.status?.state;
const taskText = (task) =>
  (task?.status?.message?.parts ?? []).filter((p) => p.kind === 'text').map((p) => p.text).join('');

async function main() {
  await withServer('probe', PORT, async (base) => {
    // ---- Arm 1: text-as-ALLOW ---------------------------------------------
    const c1 = `a2a-allow-${randomBytes(4).toString('hex')}`;
    let t = await sendMsg(base, c1, 'record this note');
    assert.equal(stateOf(t), 'input-required', `fresh turn awaits -> input-required (got ${stateOf(t)})`);
    pass('A2A: mutating tool awaits -> Task.state=input-required');

    // Any text on the awaiting context is read as an approval — the run resumes and
    // completes. There is NO way to encode a deny here.
    t = await sendMsg(base, c1, 'this text is not a deny, it approves');
    assert.equal(stateOf(t), 'completed', `text resumes as allow -> completed (got ${stateOf(t)})`);
    assert.ok(taskText(t).includes('done'), `run completed after the implicit allow: ${taskText(t)}`);
    pass('A2A: message/send text on an awaiting approval reads as ALLOW -> run completes');

    // ---- Arm 2: cancel-as-DENY --------------------------------------------
    const c2 = `a2a-deny-${randomBytes(4).toString('hex')}`;
    t = await sendMsg(base, c2, 'record this other note');
    assert.equal(stateOf(t), 'input-required', 'second context awaits too');
    const awaitingTaskId = t.id;
    assert.ok(awaitingTaskId, 'the awaiting A2A task has a server-issued id');

    // The only protocol-native rejection: tasks/cancel denies the awaiting tool.
    const cancelled = await rpc(base, 'tasks/cancel', { id: awaitingTaskId });
    assert.equal(stateOf(cancelled), 'canceled', `tasks/cancel denies + cancels (got ${stateOf(cancelled)})`);
    pass('A2A: tasks/cancel is the ONLY in-band deny -> Task.state=canceled');

    // Cancelling a context with nothing awaiting is not a false-cancel.
    const idle = `a2a-idle-${randomBytes(4).toString('hex')}`;
    await sendMsg(base, idle, 'hi'); // probe awaits, so drive it to completion:
    const completed = await sendMsg(base, idle, 'approve');
    assert.ok(completed.id, 'the completed A2A task has a server-issued id');
    const notAwaiting = await rpc(base, 'tasks/cancel', { id: completed.id }, { allowError: true });
    assert.equal(notAwaiting.error?.code, -32002, 'a terminal task is explicitly not cancelable');
    pass('A2A: cancel with nothing awaiting fails closed as task-not-cancelable');
  });

  console.log('E2E PASS: A2A HITL asymmetry (text=allow, only tasks/cancel=deny).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
