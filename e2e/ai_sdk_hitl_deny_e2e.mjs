// AI SDK HITL DENY path e2e (the gap the approve-only ai_sdk_e2e leaves open).
// A mutating tool awaits (state `approval-requested`); the client answers with the
// AI SDK native denied approval response. The real server decodes it
// to a Cancel resume, the tool is NOT run, and the run still completes.
//
// Both approval decisions use the official v7 `Chat` client surface. Real server,
// real router/decoder, and real runtime remain in the loop.
//
// Run: (from e2e/)  npm install && node ai_sdk_hitl_deny_e2e.mjs

import assert from 'node:assert/strict';
import { DefaultChatTransport, lastAssistantMessageIsCompleteWithApprovalResponses } from 'ai';
import { Chat } from '@ai-sdk/react';
import { withRealServer, pass } from './harness.mjs';

const THREAD = 'sdk-hitl-deny';
const NOTE = 'DENY-ME-NOTE';

function runsUrl(base, thread) {
  return `${base}/v1/ai-sdk/threads/${thread}/runs`;
}

function replyText(chat) {
  return (chat.lastMessage?.parts ?? [])
    .filter((part) => part.type === 'text')
    .map((part) => part.text)
    .join('');
}

async function settle(chat, awaitingMessageId) {
  for (let i = 0; i < 50; i++) {
    if (chat.status === 'ready' && chat.lastMessage?.id !== awaitingMessageId) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

async function main() {
  await withRealServer('probe', 38147, async (base) => {
    // Turn 1: drive the real await through the official Chat client.
    const chat = new Chat({
      id: THREAD,
      transport: new DefaultChatTransport({ api: runsUrl(base, THREAD) }),
      sendAutomaticallyWhen: lastAssistantMessageIsCompleteWithApprovalResponses,
    });
    await chat.sendMessage({ text: NOTE });
    const toolPart = (chat.lastMessage?.parts ?? []).find((p) => p.toolCallId);
    assert.ok(toolPart, `expected an awaiting tool part: ${JSON.stringify(chat.lastMessage?.parts)}`);
    assert.equal(toolPart.state, 'approval-requested', 'the mutating tool should await a decision');
    pass('ai-sdk deny: mutating tool awaiting native approval');

    // Turn 2: deny through the official client. It emits the native
    // `approval-responded` state and automatically submits the resume request.
    const awaitingMessageId = chat.lastMessage.id;
    await chat.addToolApprovalResponse({
      id: toolPart.approval.id,
      approved: false,
      reason: 'denied by the user',
    });
    await settle(chat, awaitingMessageId);
    const text = replyText(chat);

    // The run resumed and reached a terminal turn even though the tool was denied.
    assert.ok(text.includes('done'), `expected completion after deny, got: ${JSON.stringify(text)}`);
    // The write was blocked, so nothing echoes the note back.
    assert.ok(!text.includes(NOTE), `deny must block the write; leaked note in: ${JSON.stringify(text)}`);
    pass('ai-sdk deny: tool blocked, run still completes (await -> deny -> complete)');
  });

  console.log('E2E PASS: AI SDK HITL deny round-trip via the native approval API.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
