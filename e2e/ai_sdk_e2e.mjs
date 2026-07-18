// AI SDK protocol e2e via the official Vercel AI SDK's native client API: the
// framework-agnostic `Chat` class (from @ai-sdk/react) over `DefaultChatTransport`.
// The `Chat` manages the message history, ids, and wire shape itself — the test
// only calls `sendMessage`/`addToolResult`, never hand-builds message JSON. Covers
// multi-turn, multimodal, streaming tool calls, and HITL. Run: (from e2e/)
// npm install && node ai_sdk_e2e.mjs

import assert from 'node:assert/strict';
import { DefaultChatTransport, lastAssistantMessageIsCompleteWithToolCalls } from 'ai';
import { Chat } from '@ai-sdk/react';
import { withRealServer, pass, RED_PNG_DATA_URI } from './harness.mjs';

function newChat(base, thread, extra = {}) {
  return new Chat({
    id: thread,
    transport: new DefaultChatTransport({ api: `${base}/v1/ai-sdk/threads/${thread}/runs` }),
    ...extra,
  });
}

function replyText(chat) {
  return (chat.lastMessage?.parts ?? [])
    .filter((p) => p.type === 'text')
    .map((p) => p.text)
    .join('');
}

/// Wait for the Chat to settle back to `ready` (its auto-send may run a follow-up).
async function settle(chat) {
  for (let i = 0; i < 50 && chat.status !== 'ready'; i++) {
    await new Promise((r) => setTimeout(r, 100));
  }
}

async function main() {
  // --- multi-turn: the Chat threads history across native sendMessage calls ---
  await withRealServer('echo', 38141, async (base) => {
    const chat = newChat(base, 'sdk-mt');
    await chat.sendMessage({ text: 'first message' });
    assert.ok(replyText(chat).includes('first message'), `turn 1: ${replyText(chat)}`);
    await chat.sendMessage({ text: 'second message' });
    assert.ok(replyText(chat).includes('second message'), `turn 2: ${replyText(chat)}`);
    pass('ai-sdk multi-turn conversation');
  });

  // --- multimodal: an image attached via the native `files` param reaches the model ---
  await withRealServer('vision', 38142, async (base) => {
    const chat = newChat(base, 'sdk-img');
    await chat.sendMessage({
      text: 'what color is this',
      files: [{ type: 'file', mediaType: 'image/png', url: RED_PNG_DATA_URI }],
    });
    assert.ok(replyText(chat).includes('image/png'), `image did not reach the model: ${replyText(chat)}`);
    pass('ai-sdk multimodal (image reached the model)');
  });

  // --- streaming tool calls: the model's tool call arrives as a `tool-*` part in
  // the Chat's message state (state `input-available`), delivered mid-stream ---
  await withRealServer('probe', 38144, async (base) => {
    const chat = newChat(base, 'sdk-stream');
    await chat.sendMessage({ text: 'remember' });
    const toolPart = (chat.lastMessage?.parts ?? []).find((p) => p.toolCallId);
    assert.ok(toolPart, `expected a streamed tool part: ${JSON.stringify(chat.lastMessage?.parts)}`);
    assert.ok(toolPart.type.startsWith('tool-'), `unexpected tool part type: ${toolPart.type}`);
    assert.equal(toolPart.state, 'input-available', 'the tool call should stream its input');
    assert.ok(toolPart.input && 'path' in toolPart.input, 'the streamed tool call carries its input');
    pass('ai-sdk streaming tool call (tool-input-available)');
  });

  // --- HITL: a tool needing approval awaits; `Chat.addToolResult` submits the
  // decision and (via sendAutomaticallyWhen) auto-resends, completing the run ---
  await withRealServer('probe', 38143, async (base) => {
    const chat = newChat(base, 'sdk-hitl', {
      sendAutomaticallyWhen: lastAssistantMessageIsCompleteWithToolCalls,
    });
    await chat.sendMessage({ text: 'remember this note' });
    const toolPart = (chat.lastMessage?.parts ?? []).find((p) => p.toolCallId);
    assert.ok(toolPart, `expected an awaiting tool part: ${JSON.stringify(chat.lastMessage?.parts)}`);
    assert.equal(toolPart.state, 'input-available', 'the tool should await a decision');

    await chat.addToolResult({
      tool: toolPart.type.replace(/^tool-/, ''),
      toolCallId: toolPart.toolCallId,
      output: 'approved',
    });
    await settle(chat);
    assert.ok(replyText(chat).includes('done'), `expected completion after approval: ${replyText(chat)}`);
    pass('ai-sdk HITL approval (await -> addToolResult -> complete)');
  });

  console.log(
    'E2E PASS: AI SDK multi-turn + multimodal + streaming tool calls + HITL via the native Chat API.',
  );
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
