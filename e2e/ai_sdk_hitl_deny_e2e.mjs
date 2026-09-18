// AI SDK native-client HITL deny path. This is the deny rule paired with the
// approve rule in ai_sdk_e2e.mjs; both use the same official Chat contract and
// the server's one approval-response decoder.
//
// Run: (from e2e/) npm install && node ai_sdk_hitl_deny_e2e.mjs

import assert from 'node:assert/strict';
import {
  DefaultChatTransport,
  lastAssistantMessageIsCompleteWithApprovalResponses,
} from 'ai';
import { Chat } from '@ai-sdk/react';
import {
  createCrossProtocolApplicationThread,
  pass,
  publishAlwaysAskManagementProbeAgent,
  waitForChatSettled,
  withScenarioServer,
} from './harness.mjs';

const NOTE = 'DENY-ME-NOTE';

function replyText(chat) {
  return (chat.lastMessage?.parts ?? [])
    .filter((part) => part.type === 'text')
    .map((part) => part.text)
    .join('');
}

async function main() {
  await withScenarioServer('management-probe', 'probe', Number(process.env.E2E_PORT), async (base) => {
    await publishAlwaysAskManagementProbeAgent(base, 'assistant', ['write'], ['read']);
    const { threadId, headers } = await createCrossProtocolApplicationThread(base);
    const rawResponses = [];
    const transport = new DefaultChatTransport({
      api: `${base}/v1/ai-sdk/threads/${threadId}/runs`,
      headers,
      fetch: async (...args) => {
        const response = await fetch(...args);
        rawResponses.push(response.clone().text());
        return response;
      },
    });
    const chat = new Chat({
      id: threadId,
      transport,
      sendAutomaticallyWhen: lastAssistantMessageIsCompleteWithApprovalResponses,
    });
    await chat.sendMessage({ text: NOTE });
    const toolPart = (chat.lastMessage?.parts ?? []).find((part) => part.toolCallId);

    // Cause/effect graph: published Agent + frozen write=AlwaysAsk policy -> approval-requested;
    // explicit deny -> approval-responded(false) -> Cancel resume -> no write;
    // the run remains recoverable and reaches a terminal assistant response.
    // Decision table:
    // | wait exists | explicit decision | tool effect | terminal effect |
    // | yes + frozen AlwaysAsk | deny    | blocked     | `done`          | (R2)
    // R1 (allow -> write -> done) is owned by ai_sdk_e2e.mjs.
    assert.ok(toolPart, `expected an awaiting tool part: ${JSON.stringify(chat.lastMessage?.parts)}`);
    assert.equal(
      toolPart.state,
      'approval-requested',
      `the write must await explicit permission: parts=${JSON.stringify(chat.lastMessage?.parts)} wire=${await rawResponses[0]}`,
    );
    assert.ok(toolPart.approval?.id, 'the approval request carries a stable id');

    await chat.addToolApprovalResponse({
      id: toolPart.approval.id,
      approved: false,
      reason: 'operator denied the write',
    });
    await waitForChatSettled(
      chat,
      () => replyText(chat).includes('done'),
      "the denied tool's cancelled run never reached its terminal 'done' text",
    );
    assert.ok(replyText(chat).includes('done'), `expected completion after deny: ${replyText(chat)}`);
    assert.ok(!replyText(chat).includes(NOTE), `deny must block the write: ${replyText(chat)}`);
    pass('ai-sdk HITL deny (await -> addToolApprovalResponse(false) -> blocked -> complete)');
  });

  console.log('E2E PASS: AI SDK HITL deny round-trip via the official native Chat API.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
