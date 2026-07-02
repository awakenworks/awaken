// AI SDK protocol e2e via the official Vercel `ai` package (DefaultChatTransport +
// readUIMessageStream). Covers multi-turn (the thread carries history to the
// model) and multimodal (an image file part reaches the model). Run: (from e2e/)
// npm install && node ai_sdk_e2e.mjs

import assert from 'node:assert/strict';
import { DefaultChatTransport, readUIMessageStream } from 'ai';
import { withServer, pass, RED_PNG_DATA_URI } from './harness.mjs';

/// Send one turn's message list to the thread and return the final assistant
/// UIMessage (its parts hold text and any tool parts).
async function turnMessage(base, thread, messages) {
  const transport = new DefaultChatTransport({
    api: `${base}/v1/ai-sdk/threads/${thread}/runs`,
  });
  const stream = await transport.sendMessages({
    chatId: thread,
    messageId: messages[messages.length - 1].id,
    trigger: 'submit-user-message',
    messages,
  });
  let final;
  for await (const message of readUIMessageStream({ stream })) final = message;
  return final ?? { parts: [] };
}

function textOf(message) {
  return (message.parts ?? [])
    .filter((p) => p.type === 'text')
    .map((p) => p.text)
    .join('');
}

async function turn(base, thread, messages) {
  return textOf(await turnMessage(base, thread, messages));
}

async function main() {
  // --- multi-turn: the thread id in the URL threads history; ids dedup ---
  await withServer('echo', 38141, async (base) => {
    const u1 = { id: 'u1', role: 'user', parts: [{ type: 'text', text: 'first message' }] };
    const r1 = await turn(base, 'sdk-mt', [u1]);
    assert.ok(r1.includes('first message'), `turn 1: ${r1}`);
    const u2 = { id: 'u2', role: 'user', parts: [{ type: 'text', text: 'second message' }] };
    const r2 = await turn(base, 'sdk-mt', [u1, u2]);
    assert.ok(r2.includes('second message'), `turn 2: ${r2}`);
    pass('ai-sdk multi-turn conversation');
  });

  // --- multimodal: a `file` image part travels to the model ---
  await withServer('vision', 38142, async (base) => {
    const msg = {
      id: 'u1',
      role: 'user',
      parts: [
        { type: 'file', mediaType: 'image/png', url: RED_PNG_DATA_URI },
        { type: 'text', text: 'what color is this' },
      ],
    };
    const r = await turn(base, 'sdk-img', [msg]);
    assert.ok(r.includes('image/png'), `image did not reach the model: ${r}`);
    pass('ai-sdk multimodal (image reached the model)');
  });

  // --- HITL: a tool needing approval parks (a tool part in `input-available`);
  // approving it resumes the run to completion. The parked turn is consumed with
  // the SDK; the approval is posted at the AI SDK data-stream protocol level,
  // because the `ai` package's headless transport reserializes messages and drops
  // the custom `approval-responded` decision (its first-class HITL path is the
  // React `useChat` + `addToolApprovalResponse`, not a Node client). ---
  await withServer('probe', 38143, async (base) => {
    const api = `${base}/v1/ai-sdk/threads/sdk-hitl/runs`;
    const parked = await turnMessage(base, 'sdk-hitl', [
      { id: 'u1', role: 'user', parts: [{ type: 'text', text: 'remember this note' }] },
    ]);
    const toolPart = (parked.parts ?? []).find((p) => p.toolCallId);
    assert.ok(toolPart, `expected a parked tool part: ${JSON.stringify(parked.parts)}`);
    assert.equal(toolPart.state, 'input-available', 'the tool should await a decision');

    const resp = await fetch(api, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: 'sdk-hitl',
        messages: [
          {
            id: 'a-approve',
            role: 'assistant',
            parts: [
              {
                type: toolPart.type,
                toolCallId: toolPart.toolCallId,
                state: 'approval-responded',
                approval: { approved: true },
              },
            ],
          },
        ],
      }),
    });
    const body = await resp.text();
    assert.ok(body.includes('done'), `expected the run to finish after approval: ${body}`);
    pass('ai-sdk HITL approval (park via SDK -> approve -> complete)');
  });

  console.log('E2E PASS: AI SDK multi-turn + multimodal + HITL via the `ai` package.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
