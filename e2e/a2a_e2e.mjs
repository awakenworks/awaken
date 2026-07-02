// A2A protocol e2e via the official @a2a-js/sdk `A2AClient` (JSON-RPC transport,
// resolved from the agent card). Covers multi-turn (the contextId threads history
// to the model) and multimodal (a file part reaches the model). Run: (from e2e/)
// npm install && node a2a_e2e.mjs

import assert from 'node:assert/strict';
import { A2AClient } from '@a2a-js/sdk/client';
import { withServer, pass, RED_PNG_B64 } from './harness.mjs';

function replyText(res) {
  // `message/send` returns a Task; the agent's turn is its status message.
  const parts = res.result?.status?.message?.parts ?? [];
  return parts
    .filter((p) => p.kind === 'text')
    .map((p) => p.text)
    .join('');
}

async function main() {
  // --- multi-turn: a shared contextId threads the conversation ---
  await withServer('echo', 38151, async (base) => {
    const client = await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`);
    const r1 = await client.sendMessage({
      message: {
        messageId: 'm1',
        contextId: 'a2a-mt',
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'text', text: 'first message' }],
      },
    });
    assert.ok(replyText(r1).includes('first message'), `turn 1: ${replyText(r1)}`);
    const r2 = await client.sendMessage({
      message: {
        messageId: 'm2',
        contextId: 'a2a-mt',
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'text', text: 'second message' }],
      },
    });
    assert.ok(replyText(r2).includes('second message'), `turn 2: ${replyText(r2)}`);
    pass('a2a multi-turn conversation');
  });

  // --- HITL: a tool needing approval parks the task (input-required); a follow-up
  // message on the same context approves it and the task completes ---
  await withServer('probe', 38153, async (base) => {
    const client = await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`);
    const parked = await client.sendMessage({
      message: {
        messageId: 'm1',
        contextId: 'a2a-hitl',
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'text', text: 'remember this note' }],
      },
    });
    assert.equal(
      parked.result?.status?.state,
      'input-required',
      `expected the write tool to park: ${JSON.stringify(parked.result?.status)}`,
    );
    const done = await client.sendMessage({
      message: {
        messageId: 'm2',
        contextId: 'a2a-hitl',
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'text', text: 'approved' }],
      },
    });
    assert.equal(done.result?.status?.state, 'completed', `expected completion after approval`);
    assert.ok(replyText(done).includes('done'), `expected the run to finish: ${replyText(done)}`);
    pass('a2a HITL approval (park -> approve -> complete)');
  });

  // --- multimodal: a `file` image part travels to the model ---
  await withServer('vision', 38152, async (base) => {
    const client = await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`);
    const res = await client.sendMessage({
      message: {
        messageId: 'm1',
        contextId: 'a2a-img',
        role: 'user',
        kind: 'message',
        parts: [
          { kind: 'file', file: { bytes: RED_PNG_B64, mimeType: 'image/png' } },
          { kind: 'text', text: 'what color is this' },
        ],
      },
    });
    assert.ok(replyText(res).includes('image/png'), `image did not reach the model: ${replyText(res)}`);
    pass('a2a multimodal (image reached the model)');
  });

  console.log('E2E PASS: A2A multi-turn + multimodal + HITL via @a2a-js/sdk.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
