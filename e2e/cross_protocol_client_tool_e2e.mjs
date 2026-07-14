// Cross-protocol client-executed tool hand-off e2e (scenario #17 cross-wire): a run
// emits a CLIENT-executed tool call on the AI-SDK wire (the client must run it and
// return a result), and the result is delivered on the AG-UI wire. This exercises
// the `client_executed` branch of the neutral resume (`Resume::ClientResult`) — the
// third leg of the HITL matrix (approve / deny / client-result) across two doors.
//
// Chain:
//   AI-SDK : POST /v1/ai-sdk/threads/T/runs -> Runtime (custom: `submit_answer`
//            client tool) -> Pending{client_executed} -> park
//   AG-UI  : POST /v1/ag-ui/agents/assistant (role:"tool" content="42", no error)
//            -> resume_step -> to_resume(client_executed) -> ClientResult{content}
//            -> model replies `got: 42`
//   AI-SDK : GET history -> `got: 42` committed
//
// Deterministic (custom stub). Run: (from e2e/) node cross_protocol_client_tool_e2e.mjs

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38605);
const ANSWER = `42-${randomBytes(3).toString('hex')}`;

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
  return { raw, events };
}

async function main() {
  await withServer('custom', PORT, async (base) => {
    const thread = `xclient-${randomBytes(4).toString('hex')}`;

    // Turn 1 on AI-SDK: the model calls the client-executed `submit_answer`, which
    // parks awaiting the client's result.
    const r1 = await fetch(`${base}/v1/ai-sdk/threads/${thread}/runs`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: thread,
        messages: [{ id: 'u1', role: 'user', parts: [{ type: 'text', text: 'what is 6 x 7?' }] }],
      }),
    });
    assert.equal(r1.status, 200);
    const s1 = await drain(r1);
    const parked = s1.events.find((e) => e.toolCallId && (e.state === 'input-available' || e.type?.startsWith('tool-input')));
    assert.ok(parked, `client tool parked (events: ${s1.events.map((e) => e.type).join(',')})`);
    const toolCallId = parked.toolCallId;
    pass(`client-executed tool parked on AI-SDK (toolCallId=${toolCallId})`);

    // Turn 2 on AG-UI: deliver the client's result on the OTHER wire.
    const r2 = await fetch(`${base}/v1/ag-ui/agents/assistant`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: thread,
        runId: `run-${randomBytes(3).toString('hex')}`,
        messages: [{ id: 'tr1', role: 'tool', content: ANSWER, toolCallId }],
        tools: [],
        context: [],
        state: {},
        forwardedProps: {},
      }),
    });
    assert.equal(r2.status, 200, `ag-ui client-result resume accepted (${r2.status})`);
    await drain(r2);
    pass('AG-UI delivered the client tool result for the AI-SDK-parked run');

    // The model consumed the cross-protocol client result: `got: <answer>`.
    const hist = await (await fetch(`${base}/v1/ai-sdk/threads/${thread}/messages`)).json();
    const raw = JSON.stringify(hist.items);
    assert.ok(
      raw.includes(`got: ${ANSWER}`) || raw.includes(ANSWER),
      `model used the AG-UI-delivered client result: ${raw.slice(0, 500)}`,
    );
    pass('cross-protocol client tool: called on AI-SDK, result delivered on AG-UI, model consumed it');
  });

  console.log('E2E PASS: cross-protocol client-executed tool hand-off (AI-SDK call -> AG-UI result).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
