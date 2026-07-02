// AI SDK protocol e2e via the official Vercel `ai` package (DefaultChatTransport +
// readUIMessageStream). Covers multi-turn (the thread carries history to the
// model) and multimodal (an image file part reaches the model). Run: (from e2e/)
// npm install && node ai_sdk_e2e.mjs

import assert from 'node:assert/strict';
import { DefaultChatTransport, lastAssistantMessageIsCompleteWithToolCalls, readUIMessageStream } from 'ai';
import { Chat } from '@ai-sdk/react';
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

  // --- streaming tool calls: the model's tool call is delivered through the
  // stream as a `tool-*` part (state `input-available`), not buffered to the end ---
  await withServer('probe', 38144, async (base) => {
    const parked = await turnMessage(base, 'sdk-stream', [
      { id: 'u1', role: 'user', parts: [{ type: 'text', text: 'remember' }] },
    ]);
    const toolPart = (parked.parts ?? []).find((p) => p.toolCallId);
    assert.ok(toolPart, `expected a streamed tool part: ${JSON.stringify(parked.parts)}`);
    assert.ok(toolPart.type.startsWith('tool-'), `unexpected tool part type: ${toolPart.type}`);
    assert.equal(toolPart.state, 'input-available', 'the tool call should stream its input');
    assert.ok(toolPart.input && 'path' in toolPart.input, 'the streamed tool call carries its input');
    pass('ai-sdk streaming tool call (tool-input-available)');
  });

  // --- HITL: a tool needing approval parks (a tool part in `input-available`);
  // the SDK's `Chat` submits the decision via `addToolResult`, which auto-resends
  // (sendAutomaticallyWhen) and the run completes. Fully SDK-driven. ---
  await withServer('probe', 38143, async (base) => {
    const chat = new Chat({
      id: 'sdk-hitl',
      transport: new DefaultChatTransport({ api: `${base}/v1/ai-sdk/threads/sdk-hitl/runs` }),
      sendAutomaticallyWhen: lastAssistantMessageIsCompleteWithToolCalls,
    });
    await chat.sendMessage({ text: 'remember this note' });
    const toolPart = (chat.lastMessage.parts ?? []).find((p) => p.toolCallId);
    assert.ok(toolPart, `expected a parked tool part: ${JSON.stringify(chat.lastMessage.parts)}`);
    assert.equal(toolPart.state, 'input-available', 'the tool should await a decision');

    // Approve: hand the tool its result; the Chat auto-resends the transcript.
    await chat.addToolResult({
      tool: toolPart.type.replace(/^tool-/, ''),
      toolCallId: toolPart.toolCallId,
      output: 'approved',
    });
    for (let i = 0; i < 50 && chat.status !== 'ready'; i++) {
      await new Promise((r) => setTimeout(r, 100));
    }
    const text = (chat.lastMessage.parts ?? [])
      .filter((p) => p.type === 'text')
      .map((p) => p.text)
      .join('');
    assert.ok(text.includes('done'), `expected the run to finish after approval: ${text}`);
    pass('ai-sdk HITL approval (park -> addToolResult -> complete)');
  });

  console.log(
    'E2E PASS: AI SDK multi-turn + multimodal + streaming tool calls + HITL via the `ai` package.',
  );
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
