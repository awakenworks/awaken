// AG-UI protocol e2e via the official @ag-ui/client `HttpAgent`. Covers multi-turn
// (the agent threads history to the model) and multimodal (an image input part
// reaches the model). Run: (from e2e/) npm install && node ag_ui_e2e.mjs

import assert from 'node:assert/strict';
import { HttpAgent } from '@ag-ui/client';
import { withServer, pass, RED_PNG_B64 } from './harness.mjs';

function newAgent(base) {
  return new HttpAgent({ url: `${base}/v1/ag-ui/agents/assistant` });
}

/// Run the agent's pending messages and return the assistant reply text (read from
/// the run's `newMessages`, which the client also appends to `agent.messages`).
async function reply(agent) {
  const res = await agent.runAgent();
  const produced = res?.newMessages ?? [];
  const last = produced[produced.length - 1];
  assert.ok(last && last.role === 'assistant', `expected an assistant reply, got ${last?.role}`);
  return typeof last.content === 'string'
    ? last.content
    : (last.content ?? []).map((c) => c.text ?? '').join('');
}

async function main() {
  // --- multi-turn: the same agent instance keeps its threadId across runs ---
  await withServer('echo', 38121, async (base) => {
    const agent = newAgent(base);
    agent.messages = [{ id: 'u1', role: 'user', content: 'first message' }];
    const r1 = await reply(agent);
    assert.ok(r1.includes('first message'), `turn 1: ${r1}`);
    agent.messages.push({ id: 'u2', role: 'user', content: 'second message' });
    const r2 = await reply(agent);
    assert.ok(r2.includes('second message'), `turn 2: ${r2}`);
    pass('ag-ui multi-turn conversation');
  });

  // --- multimodal: an image input part travels to the model ---
  await withServer('vision', 38122, async (base) => {
    const agent = newAgent(base);
    agent.messages = [
      {
        id: 'u1',
        role: 'user',
        content: [
          { type: 'image', source: { type: 'data', value: RED_PNG_B64, mimeType: 'image/png' } },
          { type: 'text', text: 'what color is this' },
        ],
      },
    ];
    const r = await reply(agent);
    assert.ok(r.includes('image/png'), `image did not reach the model: ${r}`);
    pass('ag-ui multimodal (image reached the model)');
  });

  // --- HITL: a tool needing approval parks; delivering its result (approval) as a
  // `role: "tool"` message resumes the run to completion ---
  await withServer('probe', 38123, async (base) => {
    const agent = newAgent(base);
    agent.messages = [{ id: 'u1', role: 'user', content: 'remember this note' }];
    const r1 = await agent.runAgent();
    const call = (r1.newMessages ?? []).flatMap((m) => m.toolCalls ?? [])[0];
    assert.ok(call, `expected a parked tool call: ${JSON.stringify(r1.newMessages)}`);

    agent.messages = [
      ...agent.messages,
      ...r1.newMessages,
      { id: 't1', role: 'tool', toolCallId: call.id, content: 'approved' },
    ];
    const r2 = await agent.runAgent();
    const text = (r2.newMessages ?? [])
      .map((m) => (typeof m.content === 'string' ? m.content : ''))
      .join('');
    assert.ok(text.includes('done'), `expected the run to finish after approval: ${text}`);
    pass('ag-ui HITL approval (park -> approve -> complete)');
  });

  console.log('E2E PASS: AG-UI multi-turn + multimodal + HITL via @ag-ui/client.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
