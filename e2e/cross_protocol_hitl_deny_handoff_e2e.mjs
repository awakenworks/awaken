// Cross-protocol HITL DENY hand-off e2e (scenario #1 mirror / #83 deny path): a turn
// awaits on a mutating tool on the AI-SDK wire and is DENIED on the AG-UI wire. The
// AG-UI `ToolMessage.error` field maps to a neutral `Confirm{allow:false}`, so the
// tool never runs, yet the run still drives to a terminal turn.
//
// Chain:
//   AI-SDK : POST /v1/ai-sdk/threads/T/runs -> Runtime (probe mutating `write`)
//            -> PermissionGate Suspend -> await
//   AG-UI  : POST /v1/ag-ui/agents/assistant (role:"tool" result WITH `error`)
//            -> resume_step -> to_resume(error=Some) -> Confirm{allow:false}
//            -> the write is blocked -> run completes anyway
//   AI-SDK : GET history -> terminal transcript, write effect absent
//
// Deterministic (probe stub). Run: (from e2e/) node cross_protocol_hitl_deny_handoff_e2e.mjs

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38604);
const SENTINEL = `SECRET-NOTE-${randomBytes(4).toString('hex')}`;

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
      /* ignore keep-alives */
    }
  }
  return { raw, events };
}

async function main() {
  await withServer('probe', PORT, async (base) => {
    const thread = `xdeny-${randomBytes(4).toString('hex')}`;

    // Turn 1 on AI-SDK: probe writes the sentinel via the mutating `write` tool,
    // which awaits for approval.
    const r1 = await fetch(`${base}/v1/ai-sdk/threads/${thread}/runs`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: thread,
        messages: [{ id: 'u1', role: 'user', parts: [{ type: 'text', text: SENTINEL }] }],
      }),
    });
    assert.equal(r1.status, 200);
    const s1 = await drain(r1);
    const awaiting = s1.events.find((e) => e.toolCallId && (e.state === 'input-available' || e.type?.startsWith('tool-input')));
    assert.ok(awaiting, `awaiting on a tool (events: ${s1.events.map((e) => e.type).join(',')})`);
    const toolCallId = awaiting.toolCallId;
    pass(`awaiting on AI-SDK write tool (toolCallId=${toolCallId})`);

    // Turn 2 on AG-UI: DENY by including the `error` field on the tool message.
    const r2 = await fetch(`${base}/v1/ag-ui/agents/assistant`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: thread,
        runId: `run-${randomBytes(3).toString('hex')}`,
        messages: [{ id: 'tr1', role: 'tool', content: '', toolCallId, error: 'operator denied the write' }],
        tools: [],
        context: [],
        state: {},
        forwardedProps: {},
      }),
    });
    assert.equal(r2.status, 200, `ag-ui deny resume accepted (${r2.status})`);
    await drain(r2);
    pass('AG-UI carried a deny (ToolMessage.error) for the AI-SDK-awaiting tool');

    // The run reached a terminal turn, but the denied write never took effect:
    // the sentinel content the probe tried to persist must not appear as a
    // committed tool RESULT (a successful read-back would echo it).
    const hist = await (await fetch(`${base}/v1/ai-sdk/threads/${thread}/messages`)).json();
    const raw = JSON.stringify(hist.items);
    assert.ok(raw.includes('done'), `run completed after deny: ${raw.slice(0, 400)}`);
    // The sentinel appears exactly twice — the user message and the AWAITING write's
    // call arguments — but NOT a third time: a write that actually executed would add
    // a read-back tool result echoing the file content. Two occurrences == the write
    // was blocked (verified against the allow baseline in cross_protocol_cancel_e2e).
    const occurrences = raw.split(SENTINEL).length - 1;
    assert.equal(occurrences, 2, `deny blocked the write (no read-back): sentinel occurrences=${occurrences} (expected 2)`);
    pass('cross-protocol deny: awaiting on AI-SDK, denied on AG-UI, write blocked (no read-back), run still completes');
  });

  console.log('E2E PASS: cross-protocol HITL deny hand-off (AI-SDK await -> AG-UI deny -> blocked + complete).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
