// Cross-protocol cancel boundary e2e (scenario #4): a run AWAITS on a tool
// approval on the AI-SDK wire, then an A2A caller attempts to cancel it using the
// old predictable `task-${thread}` convention. A2A task ids are now opaque,
// server-issued capabilities, so guessing one from a neutral thread id must fail
// closed and must not mutate the awaiting run.
//
// Chain:
//   AI-SDK : POST /v1/ai-sdk/threads/T/runs -> Runtime (probe `write`) -> await
//   A2A    : POST /v1/a2a tasks/cancel {id: "task-T"} -> task_not_found
//   A2A    : message/send on context T -> the supported cross-protocol resume path
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
async function rpc(base, method, params, { allowError = false } = {}) {
  const res = await fetch(`${base}/v1/a2a`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: ++rpcId, method, params }),
  });
  assert.equal(res.status, 200, `${method} transport ok`);
  const body = await res.json();
  if (allowError) return body;
  assert.ok(!body.error, `${method} not a JSON-RPC error: ${JSON.stringify(body.error)}`);
  return body.result;
}

// Await a probe `write` on an AI-SDK thread carrying `note`; return the toolCallId.
async function awaitOnAiSdk(base, thread, note) {
  const r = await fetch(`${base}/v1/ai-sdk/threads/${thread}/runs`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ threadId: thread, messages: [{ id: 'u1', role: 'user', parts: [{ type: 'text', text: note }] }] }),
  });
  assert.equal(r.status, 200);
  const events = await drain(r);
  const awaiting = events.find((e) => e.toolCallId && (e.state === 'input-available' || e.type?.startsWith('tool-input')));
  assert.ok(awaiting, 'AI-SDK turn awaiting on the write tool');
  return awaiting.toolCallId;
}

// Count how many times `note` appears in the committed AI-SDK history JSON.
async function noteOccurrences(base, thread, note) {
  const hist = await (await fetch(`${base}/v1/ai-sdk/threads/${thread}/messages`)).json();
  return JSON.stringify(hist.items).split(note).length - 1;
}

async function main() {
  await withServer('probe', PORT, async (base) => {
    // --- The DENY thread: await on AI-SDK, cancel via A2A -------------------
    const denyThread = `xcancel-deny-${randomBytes(4).toString('hex')}`;
    const denyNote = `DENY-${randomBytes(4).toString('hex')}`;
    await awaitOnAiSdk(base, denyThread, denyNote);
    pass('awaiting on AI-SDK (deny thread)');

    // A neutral thread id is not authority to manufacture an A2A task id. The
    // guessed legacy id is rejected and cannot deny/cancel the awaiting run.
    const guessed = await rpc(
      base,
      'tasks/cancel',
      { id: `task-${denyThread}` },
      { allowError: true },
    );
    assert.equal(guessed.error?.code, -32001, 'a guessed A2A task id fails closed');
    pass('A2A tasks/cancel rejects a task id guessed from the neutral thread id');

    // The failed cancellation did not mutate the pending run; the supported A2A
    // context resume can still approve and complete it.
    const resumed = await rpc(base, 'message/send', {
      message: {
        messageId: `m-${randomBytes(4).toString('hex')}`,
        contextId: denyThread,
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'text', text: 'approve after rejected guessed cancel' }],
      },
    });
    assert.equal(resumed?.status?.state, 'completed', 'the awaiting run remained resumable');

    // --- The ALLOW baseline: await on AI-SDK, approve via A2A message/send --
    const allowThread = `xcancel-allow-${randomBytes(4).toString('hex')}`;
    const allowNote = `ALLOW-${randomBytes(4).toString('hex')}`;
    await awaitOnAiSdk(base, allowThread, allowNote);
    const approved = await rpc(base, 'message/send', {
      message: {
        messageId: `m-${randomBytes(4).toString('hex')}`,
        contextId: allowThread,
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'text', text: 'approve' }],
      },
    });
    assert.equal(approved?.status?.state, 'completed', `A2A message/send approved the awaiting run (got ${approved?.status?.state})`);
    pass('A2A message/send approved the AI-SDK-awaiting run (allow baseline)');

    // Both writes execute only after an explicit supported resume. The guessed
    // cancellation neither performs nor suppresses either write.
    const denyN = await noteOccurrences(base, denyThread, denyNote);
    const allowN = await noteOccurrences(base, allowThread, allowNote);
    assert.ok(denyN > 2, `first write executed only after supported resume (got ${denyN})`);
    assert.ok(allowN > 2, `allow baseline executed the write (got ${allowN})`);
    pass('guessed cancellation was side-effect free; both explicit resumes completed');
  });

  console.log('E2E PASS: cross-protocol cancel boundary rejects guessed A2A task ids fail-closed.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
