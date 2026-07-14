// Multi-turn interleaved cross-protocol e2e (scenario #3 extended to multi-turn):
// SIX sequential turns on ONE thread, the wire ROTATING every turn
// (AI-SDK → AG-UI → A2A → …). Each turn carries a unique ordered marker. The final
// committed transcript — read back through every wire — must contain all six
// markers IN ORDER, proving that interleaving protocols across a conversation
// appends to one ordered `SharedHost` thread with no loss or reordering.
//
// Chain (per turn, rotating): {AI-SDK POST runs | AG-UI POST agents | A2A message/send}
//   -> ProtocolHost -> SharedHost -> Runtime (echo) -> commit_run (append, thread T)
// Final reads: GET /v1/ai-sdk/threads/T/messages, GET /v1/ag-ui/threads/T/messages,
//              A2A tasks/get {id: task-T}.
//
// Deterministic (echo). Run: (from e2e/) node cross_protocol_multiturn_e2e.mjs

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38611);
const TURNS = 6;

let rpcId = 0;
async function rpc(base, method, params) {
  const res = await fetch(`${base}/v1/a2a`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: ++rpcId, method, params }),
  });
  assert.equal(res.status, 200, `${method} transport ok`);
  const body = await res.json();
  assert.ok(!body.error, `${method}: ${JSON.stringify(body.error)}`);
  return body.result;
}

async function drainSse(res) {
  assert.equal(res.status, 200, `turn accepted (${res.status})`);
  await res.text();
}

// One turn on the named wire, carrying `marker` on thread `t`.
async function turn(base, wire, t, marker, i) {
  if (wire === 'ai-sdk') {
    await drainSse(
      await fetch(`${base}/v1/ai-sdk/threads/${t}/runs`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ threadId: t, messages: [{ id: `u${i}`, role: 'user', parts: [{ type: 'text', text: marker }] }] }),
      }),
    );
  } else if (wire === 'ag-ui') {
    await drainSse(
      await fetch(`${base}/v1/ag-ui/agents/assistant`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          threadId: t,
          runId: `run-${i}-${randomBytes(2).toString('hex')}`,
          messages: [{ id: `u${i}`, role: 'user', content: marker }],
          tools: [],
          context: [],
          state: {},
          forwardedProps: {},
        }),
      }),
    );
  } else {
    await rpc(base, 'message/send', {
      message: { messageId: `u${i}`, contextId: t, role: 'user', kind: 'message', parts: [{ kind: 'text', text: marker }] },
    });
  }
}

// Assert markers appear in ascending order in a serialized history blob.
function assertOrdered(raw, markers, where) {
  let last = -1;
  for (const m of markers) {
    const at = raw.indexOf(m);
    assert.ok(at >= 0, `${where}: marker ${m} present`);
    assert.ok(at > last, `${where}: marker ${m} appears after the previous one (order preserved)`);
    last = at;
  }
}

async function main() {
  await withServer('echo', PORT, async (base) => {
    const thread = `xmulti-${randomBytes(4).toString('hex')}`;
    const wires = ['ai-sdk', 'ag-ui', 'a2a'];
    const markers = [];

    for (let i = 0; i < TURNS; i++) {
      const wire = wires[i % wires.length];
      const marker = `T${i}-${randomBytes(3).toString('hex')}`;
      markers.push(marker);
      await turn(base, wire, thread, marker, i);
      pass(`turn ${i} via ${wire} (${marker})`);
    }

    // Final transcript, read back through all three wires, all markers ordered.
    const ai = await (await fetch(`${base}/v1/ai-sdk/threads/${thread}/messages`)).json();
    const agui = await (await fetch(`${base}/v1/ag-ui/threads/${thread}/messages`)).json();
    const a2a = await rpc(base, 'tasks/get', { id: `task-${thread}` });

    assertOrdered(JSON.stringify(ai.items), markers, 'ai-sdk history');
    assertOrdered(JSON.stringify(agui.items), markers, 'ag-ui history');
    assertOrdered(JSON.stringify(a2a), markers, 'a2a tasks/get');
    pass(`all ${TURNS} interleaved turns present and ordered in every wire's projection`);

    // The transcript grew by at least one message per turn (append-only).
    assert.ok(ai.items.length >= TURNS, `history has >= ${TURNS} messages (got ${ai.items.length})`);
    pass('interleaving protocols across a conversation appended to one ordered thread');
  });

  console.log('E2E PASS: multi-turn interleaved cross-protocol continuity (AI-SDK/AG-UI/A2A, ordered).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
