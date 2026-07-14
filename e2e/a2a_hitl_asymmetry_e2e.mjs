// A2A HITL asymmetry e2e (scenario #5): A2A has NO in-band "deny" for a built-in
// tool approval. `message/send` on a parked run reads ANY text as an ALLOW; the only
// protocol-native rejection is `tasks/cancel`, which denies the parked tool and
// leaves the task `canceled`. This pins the documented HITL-matrix asymmetry that
// distinguishes A2A from AI-SDK/AG-UI/Managed.
//
// Chain (probe: the mutating `write` parks on approval):
//   POST /v1/a2a message/send (fresh)   -> Runtime park -> Task.state=input-required
//   POST /v1/a2a message/send (parked)  -> to_resume(built-in)=Confirm{allow:true}
//                                          -> run completes -> Task.state=completed  (text = ALLOW)
//   POST /v1/a2a tasks/cancel (parked)  -> Confirm{allow:false} -> Task.state=canceled  (the deny path)
//
// Deterministic (probe stub, no API key). Run: (from e2e/) node a2a_hitl_asymmetry_e2e.mjs

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38606);

let rpcId = 0;
async function rpc(base, method, params) {
  const res = await fetch(`${base}/v1/a2a`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: ++rpcId, method, params }),
  });
  assert.equal(res.status, 200, `${method} transport ok (${res.status})`);
  const body = await res.json();
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
    assert.equal(stateOf(t), 'input-required', `fresh turn parks -> input-required (got ${stateOf(t)})`);
    pass('A2A: mutating tool parks -> Task.state=input-required');

    // Any text on the parked context is read as an approval — the run resumes and
    // completes. There is NO way to encode a deny here.
    t = await sendMsg(base, c1, 'this text is not a deny, it approves');
    assert.equal(stateOf(t), 'completed', `text resumes as allow -> completed (got ${stateOf(t)})`);
    assert.ok(taskText(t).includes('done'), `run completed after the implicit allow: ${taskText(t)}`);
    pass('A2A: message/send text on a parked approval reads as ALLOW -> run completes');

    // ---- Arm 2: cancel-as-DENY --------------------------------------------
    const c2 = `a2a-deny-${randomBytes(4).toString('hex')}`;
    t = await sendMsg(base, c2, 'record this other note');
    assert.equal(stateOf(t), 'input-required', 'second context parks too');

    // The only protocol-native rejection: tasks/cancel denies the parked tool.
    const cancelled = await rpc(base, 'tasks/cancel', { id: `task-${c2}` });
    assert.equal(stateOf(cancelled), 'canceled', `tasks/cancel denies + cancels (got ${stateOf(cancelled)})`);
    pass('A2A: tasks/cancel is the ONLY in-band deny -> Task.state=canceled');

    // Cancelling a context with nothing parked is not a false-cancel.
    const idle = `a2a-idle-${randomBytes(4).toString('hex')}`;
    await sendMsg(base, idle, 'hi'); // completes immediately? probe parks, so drive it to completion:
    await sendMsg(base, idle, 'approve'); // now terminal
    const notParked = await rpc(base, 'tasks/cancel', { id: `task-${idle}` });
    assert.notEqual(stateOf(notParked), 'canceled', `cancel with nothing parked is not a false-cancel (got ${stateOf(notParked)})`);
    pass('A2A: cancel with nothing parked returns current state, not a false canceled');
  });

  console.log('E2E PASS: A2A HITL asymmetry (text=allow, only tasks/cancel=deny).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
