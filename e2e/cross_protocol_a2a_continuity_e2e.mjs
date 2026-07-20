// Three-wire thread continuity e2e (scenario #2 extended to A2A): A2A addresses a
// thread by `contextId`; AI-SDK/AG-UI by their URL thread id. All three project the
// same `rt.history(thread)` over one `SharedHost`. So a turn committed via A2A is
// readable through AI-SDK and AG-UI, and a subsequent AI-SDK turn on the same id is
// visible back through A2A `tasks/get`.
//
// Chain:
//   A2A     : POST /v1/a2a message/send (contextId=C)  -> commit thread C
//   AI-SDK  : GET /v1/ai-sdk/threads/C/messages         -> rt.history(C)
//   AG-UI   : GET /v1/ag-ui/threads/C/messages          -> rt.history(C)
//   AI-SDK  : POST /v1/ai-sdk/threads/C/runs            -> second turn on C
//   A2A     : POST /v1/a2a tasks/get {id: "task-C"}     -> both turns in the Task history
//
// Deterministic (echo). Run: (from e2e/) node cross_protocol_a2a_continuity_e2e.mjs

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38610);

let rpcId = 0;
async function rpc(base, method, params) {
  const res = await fetch(`${base}/v1/a2a`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: ++rpcId, method, params }),
  });
  assert.equal(res.status, 200, `${method} transport ok`);
  const body = await res.json();
  assert.ok(!body.error, `${method} not an error: ${JSON.stringify(body.error)}`);
  return body.result;
}

function a2aSend(base, context, text) {
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

async function drainSse(res) {
  assert.equal(res.status, 200);
  return (await res.text());
}

async function messages(base, wire, thread) {
  const res = await fetch(`${base}/v1/${wire}/threads/${thread}/messages`);
  assert.equal(res.status, 200, `${wire} history ${thread} -> ${res.status}`);
  const body = await res.json();
  assert.ok(Array.isArray(body.items), `${wire} history envelope`);
  return { items: body.items, raw: JSON.stringify(body.items) };
}

async function main() {
  await withServer('echo', PORT, async (base) => {
    const ctx = `xa2a-${randomBytes(4).toString('hex')}`;
    const ALPHA = `ALPHA-${randomBytes(3).toString('hex')}`;
    const BETA = `BETA-${randomBytes(3).toString('hex')}`;

    // --- Turn 1 on the A2A wire -------------------------------------------
    const task = await a2aSend(base, ctx, ALPHA);
    const taskText = (task?.status?.message?.parts ?? []).filter((p) => p.kind === 'text').map((p) => p.text).join('');
    assert.ok(taskText.includes(ALPHA), `A2A echoed ALPHA: ${taskText}`);
    pass(`turn 1 committed via A2A on context ${ctx}`);

    // --- Read the same thread through AI-SDK and AG-UI ---------------------
    const ai1 = await messages(base, 'ai-sdk', ctx);
    assert.ok(ai1.raw.includes(ALPHA), 'A2A turn visible through the AI-SDK projection');
    const agui1 = await messages(base, 'ag-ui', ctx);
    assert.ok(agui1.raw.includes(ALPHA), 'A2A turn visible through the AG-UI projection');
    pass('A2A-committed turn readable through BOTH AI-SDK and AG-UI (one host thread)');

    // --- Turn 2 on the AI-SDK wire, SAME id -------------------------------
    const r2 = await fetch(`${base}/v1/ai-sdk/threads/${ctx}/runs`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ threadId: ctx, messages: [{ id: 'u2', role: 'user', parts: [{ type: 'text', text: BETA }] }] }),
    });
    await drainSse(r2);
    pass('turn 2 committed via AI-SDK on the same id');

    const ai2 = await messages(base, 'ai-sdk', ctx);
    const agui2 = await messages(base, 'ag-ui', ctx);
    assert.ok(ai2.raw.includes(BETA), 'AI-SDK turn remains visible through AI-SDK history');
    assert.ok(agui2.raw.includes(BETA), 'AI-SDK turn is visible through AG-UI history');

    // --- A2A task lookup uses its opaque server-issued id -----------------
    assert.ok(task?.id, 'A2A returned a server-issued task id');
    const got = await rpc(base, 'tasks/get', { id: task.id });
    const hist = JSON.stringify(got);
    assert.ok(hist.includes(ALPHA), 'A2A tasks/get still carries turn 1');
    assert.ok(!hist.includes(BETA), 'the A2A task projection remains the immutable A2A turn snapshot');
    pass('three-wire continuity: A2A <-> AI-SDK <-> AG-UI over one SharedHost thread');
  });

  console.log('E2E PASS: three-wire thread continuity (A2A <-> AI-SDK <-> AG-UI).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
