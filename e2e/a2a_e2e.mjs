// A2A protocol e2e via the official @a2a-js/sdk `A2AClient` (JSON-RPC transport,
// resolved from the agent card). Covers multi-turn (the contextId threads history
// to the model) and multimodal (a file part reaches the model). Run: (from e2e/)
// npm install && node a2a_e2e.mjs

import assert from 'node:assert/strict';
import { A2AClient } from '@a2a-js/sdk/client';
import { withRealServer, pass, RED_PNG_B64 } from './harness.mjs';

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
  await withRealServer('echo', 38151, async (base) => {
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

    // Causal graph:
    // A2A data JSON -> typed A2A Part -> runtime ACL (no generic JSON block)
    //                               `-> adjacent text -> model -> task reply
    // Decision table:
    // | part | accepted by A2A | reaches neutral prompt | observable reply |
    // | data | yes             | no                     | marker absent    |
    // | text | yes             | yes                    | text present     |
    // This drives the complete protocol/runtime path; it is not a DTO-only check.
    const withData = await client.sendMessage({
      message: {
        messageId: 'm3',
        contextId: 'a2a-data',
        role: 'user',
        kind: 'message',
        parts: [
          { kind: 'data', data: { nested: [1, true, null], marker: 'must-not-be-prompted' } },
          { kind: 'text', text: 'visible text' },
        ],
      },
    });
    assert.ok(replyText(withData).includes('visible text'), `text was lost: ${replyText(withData)}`);
    assert.ok(
      !replyText(withData).includes('must-not-be-prompted'),
      `A2A-owned JSON leaked into the neutral prompt: ${replyText(withData)}`,
    );
    pass('a2a multi-turn + data-part isolation');
  });

  // --- HITL: a tool needing approval awaits the task (input-required); a follow-up
  // message on the same context approves it and the task completes ---
  await withRealServer('probe', 38153, async (base) => {
    const client = await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`);
    const awaiting = await client.sendMessage({
      message: {
        messageId: 'm1',
        contextId: 'a2a-hitl',
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'text', text: 'remember this note' }],
      },
    });
    assert.equal(
      awaiting.result?.status?.state,
      'input-required',
      `expected the write tool to await: ${JSON.stringify(awaiting.result?.status)}`,
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
    pass('a2a HITL approval (await -> approve -> complete)');
  });

  // --- multimodal: a `file` image part travels to the model ---
  await withRealServer('vision', 38152, async (base) => {
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

  console.log('E2E PASS: A2A multi-turn + data isolation + multimodal + HITL via @a2a-js/sdk.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
