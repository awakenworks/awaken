// Cross-protocol cancel e2e (scenario #4): a run PARKS on a tool approval on the
// AI-SDK wire and is REJECTED by an A2A `tasks/cancel` on the SAME thread. A2A's
// only in-band deny (cancel) reaches across the neutral seam: it denies the parked
// tool (`Resume::Confirm{allow:false}`) and the AI-SDK-parked run reads back
// terminated with the write blocked.
//
// Chain:
//   AI-SDK : POST /v1/ai-sdk/threads/T/runs -> Runtime (probe `write`) -> park
//   A2A    : POST /v1/a2a tasks/cancel {id: "task-T"} -> rt.pending(T) ->
//            resume Confirm{allow:false} -> Task.state=canceled
//   AI-SDK : GET /v1/ai-sdk/threads/T/messages -> terminal, no read-back of the note
//
// Deterministic (probe stub). Run: (from e2e/) node cross_protocol_cancel_e2e.mjs

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38607);

async function drain(res) {
  const raw = await res.text();
  const events = [];
  for (const line of raw.split('\n')) {
    const t = line.trim();
    if (!t.startsWith('data:')) continue;
    const p = t.slice(5).trim();
    if (!p || p === '[DONE]') continue;
    try {
      events.push(JSON.parse(p));
    } catch {
      /* ignore */
    }
  }
  return events;
}

let rpcId = 0;
async function rpc(base, method, params) {
  const res = await fetch(`${base}/v1/a2a`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: ++rpcId, method, params }),
  });
  assert.equal(res.status, 200, `${method} transport ok`);
  const body = await res.json();
  assert.ok(!body.error, `${method} not a JSON-RPC error: ${JSON.stringify(body.error)}`);
  return body.result;
}

// Park a probe `write` on an AI-SDK thread carrying `note`; return the toolCallId.
async function parkOnAiSdk(base, thread, note) {
  const r = await fetch(`${base}/v1/ai-sdk/threads/${thread}/runs`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ threadId: thread, messages: [{ id: 'u1', role: 'user', parts: [{ type: 'text', text: note }] }] }),
  });
  assert.equal(r.status, 200);
  const events = await drain(r);
  const parked = events.find((e) => e.toolCallId && (e.state === 'input-available' || e.type?.startsWith('tool-input')));
  assert.ok(parked, 'AI-SDK turn parked on the write tool');
  return parked.toolCallId;
}

// Count how many times `note` appears in the committed AI-SDK history JSON.
async function noteOccurrences(base, thread, note) {
  const hist = await (await fetch(`${base}/v1/ai-sdk/threads/${thread}/messages`)).json();
  return JSON.stringify(hist.items).split(note).length - 1;
}

async function main() {
  await withServer('probe', PORT, async (base) => {
    // --- The DENY thread: park on AI-SDK, cancel via A2A -------------------
    const denyThread = `xcancel-deny-${randomBytes(4).toString('hex')}`;
    const denyNote = `DENY-${randomBytes(4).toString('hex')}`;
    await parkOnAiSdk(base, denyThread, denyNote);
    pass('parked on AI-SDK (deny thread)');

    // A2A reads the SAME thread id as its context: cancel task-<thread>.
    const task = await rpc(base, 'tasks/cancel', { id: `task-${denyThread}` });
    assert.equal(task?.status?.state, 'canceled', `A2A tasks/cancel denied+cancelled the parked run (got ${task?.status?.state})`);
    pass('A2A tasks/cancel rejected the AI-SDK-parked run across the neutral seam');

    // --- The ALLOW baseline: park on AI-SDK, approve via A2A message/send --
    const allowThread = `xcancel-allow-${randomBytes(4).toString('hex')}`;
    const allowNote = `ALLOW-${randomBytes(4).toString('hex')}`;
    await parkOnAiSdk(base, allowThread, allowNote);
    const approved = await rpc(base, 'message/send', {
      message: {
        messageId: `m-${randomBytes(4).toString('hex')}`,
        contextId: allowThread,
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'text', text: 'approve' }],
      },
    });
    assert.equal(approved?.status?.state, 'completed', `A2A message/send approved the parked run (got ${approved?.status?.state})`);
    pass('A2A message/send approved the AI-SDK-parked run (allow baseline)');

    // --- The discriminator: a note appears (user + write-call args) twice; only
    //     a run whose write ACTUALLY executed adds a third occurrence via the
    //     read-back tool result. Deny must have strictly fewer than allow. -----
    const denyN = await noteOccurrences(base, denyThread, denyNote);
    const allowN = await noteOccurrences(base, allowThread, allowNote);
    assert.equal(denyN, 2, `denied write: note appears only in the user msg + parked call args (got ${denyN})`);
    assert.ok(allowN > denyN, `approved write executed and read the note back (allow=${allowN} > deny=${denyN})`);
    pass(`cross-protocol cancel blocked the write (deny=${denyN} occurrences < allow=${allowN}): the A2A cancel truly denied it`);
  });

  console.log('E2E PASS: cross-protocol cancel (AI-SDK park -> A2A tasks/cancel -> denied; contrasted vs A2A allow).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
