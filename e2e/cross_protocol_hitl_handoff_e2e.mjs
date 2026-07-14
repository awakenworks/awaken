// Cross-protocol HITL hand-off e2e (scenario #1): a turn PARKS on a tool approval
// on ONE wire and is APPROVED + resumed to completion on ANOTHER — same thread id,
// same `SharedHost`, same parked `WaitingTicket`. The approval decision (`Resume`)
// is protocol-neutral, so a client can switch doors mid-turn.
//
// Chain:
//   AI-SDK  : POST /v1/ai-sdk/threads/T/runs -> ProtocolHost::run_streaming ->
//             SharedHost -> Runtime (probe: mutating `write`) -> PermissionGate
//             -> Suspend -> commit WaitingTicket{ToolPermission}  (parked)
//   AG-UI   : POST /v1/ag-ui/agents/assistant (threadId T, role:"tool" result, no
//             user msg) -> resume_step -> rt.pending(T) -> to_resume(Confirm{allow})
//             -> ProtocolHost::resume -> SAME parked run drives to `done`
//   AI-SDK  : GET /v1/ai-sdk/threads/T/messages -> the completed transcript
//
// Deterministic (probe stub model, no API key). Run: (from e2e/) node cross_protocol_hitl_handoff_e2e.mjs

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38603);

// Drain an SSE body into { text, events } (events = parsed data frames).
async function drain(res) {
  const raw = await res.text();
  const events = [];
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
    events.push(ev);
    if (ev.type === 'text-delta' && typeof ev.delta === 'string') text += ev.delta;
    if (ev.type === 'TEXT_MESSAGE_CONTENT' && typeof ev.delta === 'string') text += ev.delta;
  }
  return { raw, text, events };
}

async function main() {
  await withServer('probe', PORT, async (base) => {
    const thread = `xhitl-${randomBytes(4).toString('hex')}`;

    // --- Turn 1 on AI-SDK: the mutating write parks for approval ------------
    const r1 = await fetch(`${base}/v1/ai-sdk/threads/${thread}/runs`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: thread,
        messages: [{ id: 'u1', role: 'user', parts: [{ type: 'text', text: 'please record this note' }] }],
      }),
    });
    assert.equal(r1.status, 200, 'ai-sdk turn accepted');
    const s1 = await drain(r1);
    // The parked tool surfaces with a toolCallId (state input-available on the tail).
    const parked = s1.events.find((e) => e.toolCallId && (e.state === 'input-available' || e.type?.startsWith('tool-input')));
    assert.ok(parked, `ai-sdk turn parked on a tool (events: ${s1.events.map((e) => e.type).join(',')})`);
    const toolCallId = parked.toolCallId;
    assert.ok(!s1.text.includes('done'), 'run did NOT complete on the AI-SDK wire (it parked)');
    pass(`turn parked on AI-SDK awaiting approval (toolCallId=${toolCallId})`);

    // Sanity: the runtime reports a pending decision for this thread.
    // (Observed indirectly — the resume below fails closed if there is none.)

    // --- Approve + resume on AG-UI, SAME thread ----------------------------
    const r2 = await fetch(`${base}/v1/ag-ui/agents/assistant`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: thread,
        runId: `run-${randomBytes(3).toString('hex')}`,
        // A role:"tool" result with no user content => AG-UI resume path; no `error`
        // => Confirm{allow:true} for a built-in approval.
        messages: [{ id: 'tr1', role: 'tool', content: 'approved', toolCallId }],
        tools: [],
        context: [],
        state: {},
        forwardedProps: {},
      }),
    });
    assert.equal(r2.status, 200, `ag-ui resume accepted (${r2.status})`);
    const s2 = await drain(r2);
    pass('AG-UI accepted the resume for the AI-SDK-parked run');

    // --- The run completed: `done` is now in the committed transcript ------
    const hist = await fetch(`${base}/v1/ai-sdk/threads/${thread}/messages`);
    assert.equal(hist.status, 200, 'history read ok');
    const body = await hist.json();
    const raw = JSON.stringify(body.items);
    assert.ok(
      raw.includes('done'),
      `the AI-SDK-parked run reached completion after the AG-UI approval: ${raw.slice(0, 500)}`,
    );
    pass('cross-protocol HITL: parked on AI-SDK, approved on AG-UI, resumed to completion (one thread)');
  });

  console.log('E2E PASS: cross-protocol HITL hand-off (AI-SDK park -> AG-UI approve -> resume).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
