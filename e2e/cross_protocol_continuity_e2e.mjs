// Cross-protocol thread continuity e2e (scenario #1/#2): ONE `awaken-server-local`
// process, ONE thread id, driven and read back across DIFFERENT protocol front
// doors. Every adapter projects the same `rt.history(thread_id)` over the same
// `SharedHost`, keyed by thread id — so a turn committed through one wire must be
// visible, verbatim, through another.
//
// Chain per turn:
//   POST /v1/ai-sdk/threads/T/runs  -> ProtocolHost::run_streaming -> SharedHost
//     -> DirectRunIngress -> Runtime loop (echo) -> commit_run  (thread T)
//   GET  /v1/ai-sdk/threads/T/messages -> rt.history(T)  (AI-SDK projection)
//   GET  /v1/ag-ui/threads/T/messages  -> rt.history(T)  (AG-UI projection)   <-- same host, other wire
//   POST /v1/ag-ui/agents/assistant (threadId T) -> second turn on the SAME thread
//   GET  /v1/ai-sdk/threads/T/messages -> BOTH turns now visible on the first wire
//
// Deterministic (echo model, no API key). Run: (from e2e/) node cross_protocol_continuity_e2e.mjs

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38601);

// Drain an AI-SDK / AG-UI SSE body into the concatenated text-delta reply.
async function drainSse(res) {
  assert.equal(res.status, 200, `stream accepted (${res.status})`);
  const raw = await res.text();
  let text = '';
  for (const line of raw.split('\n')) {
    const t = line.trim();
    if (!t.startsWith('data:')) continue;
    const p = t.slice(5).trim();
    if (!p || p === '[DONE]') continue;
    let ev;
    try {
      ev = JSON.parse(p);
    } catch {
      continue;
    }
    if (ev.type === 'text-delta' && typeof ev.delta === 'string') text += ev.delta;
    // AG-UI live frames.
    if (ev.type === 'TEXT_MESSAGE_CONTENT' && typeof ev.delta === 'string') text += ev.delta;
  }
  return text;
}

async function getMessages(url) {
  const res = await fetch(url);
  assert.equal(res.status, 200, `history read ${url} -> ${res.status}`);
  const body = await res.json();
  assert.ok(Array.isArray(body.items), `history envelope has items[]: ${JSON.stringify(body).slice(0, 200)}`);
  return { items: body.items, raw: JSON.stringify(body.items) };
}

async function main() {
  await withServer('echo', PORT, async (base) => {
    const thread = `xproto-${randomBytes(4).toString('hex')}`;
    const ALPHA = `ALPHA-${randomBytes(3).toString('hex')}`;
    const BETA = `BETA-${randomBytes(3).toString('hex')}`;

    // --- Turn 1 on the AI-SDK wire -----------------------------------------
    const r1 = await fetch(`${base}/v1/ai-sdk/threads/${thread}/runs`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: thread,
        messages: [{ id: 'u1', role: 'user', parts: [{ type: 'text', text: ALPHA }] }],
      }),
    });
    const reply1 = await drainSse(r1);
    assert.ok(reply1.includes(ALPHA), `ai-sdk turn 1 echoed ALPHA: ${JSON.stringify(reply1)}`);
    pass(`turn 1 committed via AI-SDK on thread ${thread}`);

    // --- Read that same thread back through AI-SDK AND AG-UI ----------------
    const aiHist1 = await getMessages(`${base}/v1/ai-sdk/threads/${thread}/messages`);
    assert.ok(aiHist1.raw.includes(ALPHA), 'AI-SDK history carries the turn-1 text');

    const aguiHist1 = await getMessages(`${base}/v1/ag-ui/threads/${thread}/messages`);
    assert.ok(
      aguiHist1.raw.includes(ALPHA),
      `AG-UI projection of the SAME host thread carries turn 1 (cross-protocol read): ${aguiHist1.raw.slice(0, 300)}`,
    );
    pass('turn 1 visible through BOTH the AI-SDK and AG-UI projections of the one host thread');

    // --- Turn 2 on the AG-UI wire, SAME thread id --------------------------
    const r2 = await fetch(`${base}/v1/ag-ui/agents/assistant`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: thread,
        runId: `run-${randomBytes(3).toString('hex')}`,
        messages: [{ id: 'u2', role: 'user', content: BETA }],
        tools: [],
        context: [],
        state: {},
        forwardedProps: {},
      }),
    });
    const reply2 = await drainSse(r2);
    assert.ok(reply2.includes(BETA), `ag-ui turn 2 echoed BETA: ${JSON.stringify(reply2)}`);
    pass(`turn 2 committed via AG-UI on the SAME thread ${thread}`);

    // --- Both turns now visible on the AI-SDK wire -------------------------
    const aiHist2 = await getMessages(`${base}/v1/ai-sdk/threads/${thread}/messages`);
    assert.ok(aiHist2.raw.includes(ALPHA), 'AI-SDK history still carries turn 1');
    assert.ok(
      aiHist2.raw.includes(BETA),
      `AI-SDK history carries the AG-UI-committed turn 2 (one thread, two wires): ${aiHist2.raw.slice(0, 400)}`,
    );
    // The AG-UI turn genuinely appended, not replaced: more items than after turn 1.
    assert.ok(
      aiHist2.items.length > aiHist1.items.length,
      `thread grew across the cross-protocol turn (${aiHist1.items.length} -> ${aiHist2.items.length})`,
    );
    pass('turn started on AG-UI is observable through AI-SDK: one SharedHost thread, thread-keyed continuity');
  });

  console.log('E2E PASS: cross-protocol thread continuity (AI-SDK <-> AG-UI over one SharedHost thread).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
