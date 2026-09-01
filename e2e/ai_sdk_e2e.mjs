// AI SDK protocol e2e via the official Vercel AI SDK's native client API: the
// framework-agnostic `Chat` class (from @ai-sdk/react) over `DefaultChatTransport`.
// The `Chat` manages the message history, ids, and wire shape itself — the test
// only calls `sendMessage`/`addToolApprovalResponse`, never hand-builds message
// JSON. Covers multi-turn, multimodal, and HITL. Incremental argument framing is
// owned by streaming_tool_input_e2e.mjs rather than duplicated here. Run: (from e2e/)
// npm install && node ai_sdk_e2e.mjs

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
  RED_PNG_DATA_URI,
  withRealServer,
  withScenarioServer,
} from './harness.mjs';

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

/// Wait for the Chat's deferred auto-send to both start and reach its expected
/// effect. Testing `ready` alone races: addToolResult may resolve while the Chat
/// is still ready, immediately before the SDK schedules its follow-up request.
async function settle(chat, effect) {
  for (let i = 0; i < 50; i++) {
    if (chat.status === 'ready' && effect()) return;
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

  // --- pre-uploaded File: the official Chat preserves a custom data part; Awaken
  // resolves its opaque logical id in the Run Workspace before model I/O. ---
  await withRealServer('vision', 38144, async (base) => {
    const form = new FormData();
    form.append('file', new Blob([Buffer.from(RED_PNG_DATA_URI.split(',')[1], 'base64')], {
      type: 'image/png',
    }), 'red.png');
    const uploaded = await fetch(`${base}/v1/files?beta=true`, {
      method: 'POST',
      headers: { 'anthropic-beta': 'files-api-2025-04-14' },
      body: form,
    });
    const uploadedBody = await uploaded.text();
    assert.equal(uploaded.status, 200, uploadedBody);
    const receipt = JSON.parse(uploadedBody);
    assert.match(receipt.id, /^file_[0-9a-f]{32}$/u);

    const chat = newChat(base, 'sdk-file-ref');
    await chat.sendMessage({
      parts: [
        {
          type: 'data-awaken-file',
          data: {
            object: 'awaken.file_reference',
            fileRef: receipt.id,
            kind: 'image',
          },
        },
        { type: 'text', text: 'inspect the pre-uploaded file' },
      ],
    });
    assert.ok(
      replyText(chat).includes('image/png'),
      `logical File did not reach the model: ${replyText(chat)}`,
    );
    const history = await fetch(`${base}/v1/ai-sdk/threads/sdk-file-ref/messages`);
    assert.equal(history.status, 200);
    const replay = await history.json();
    const filePart = replay.items[0].parts.find((part) => part.type === 'data-awaken-file');
    assert.deepEqual(filePart, {
      type: 'data-awaken-file',
      data: {
        object: 'awaken.file_reference',
        fileRef: receipt.id,
        kind: 'image',
      },
    });
    assert.equal(JSON.stringify(filePart).includes('url'), false);
    pass('ai-sdk pre-uploaded logical File (reference-only request and history)');
  });

  // --- HITL: a tool needing approval awaits; `Chat.addToolApprovalResponse` submits the
  // decision and (via sendAutomaticallyWhen) auto-resends, completing the run ---
  await withScenarioServer('management-probe', 'probe', 38143, async (base) => {
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
      id: 'sdk-hitl',
      transport,
      sendAutomaticallyWhen: lastAssistantMessageIsCompleteWithApprovalResponses,
    });
    await chat.sendMessage({ text: 'remember this note' });
    const firstWire = await rawResponses[0];
    assert.ok(
      firstWire.includes('"type":"tool-approval-request"'),
      `the server must emit an approval request before the SDK can answer it: ${firstWire}`,
    );
    const toolPart = (chat.lastMessage?.parts ?? []).find((p) => p.toolCallId);
    assert.ok(toolPart, `expected an awaiting tool part: ${JSON.stringify(chat.lastMessage?.parts)}`);
    assert.ok(toolPart.type.startsWith('tool-'), `unexpected tool part type: ${toolPart.type}`);
    assert.ok(toolPart.input && 'path' in toolPart.input, 'the approval carries the parsed tool input');
    assert.equal(
      toolPart.state,
      'approval-requested',
      `the tool should await a decision: ${JSON.stringify(chat.lastMessage?.parts)}`,
    );
    assert.ok(toolPart.approval?.id, 'the approval request carries a stable id');
    await chat.addToolApprovalResponse({
      id: toolPart.approval.id,
      approved: true,
    });
    // Cause/effect graph: published Agent + frozen write=AlwaysAsk policy ->
    // committed built-in wait -> input + approval request;
    // explicit allow -> SDK approval response -> runtime resume -> terminal text.
    // Decision rule HITL-R1 covers the allow edge here; the deny edge is owned by
    // ai_sdk_hitl_deny_e2e.mjs. The polling oracle requires both terminal status
    // and terminal effect, so pre-auto-send `ready` cannot masquerade as success.
    await settle(chat, () => replyText(chat).includes('done'));
    assert.ok(replyText(chat).includes('done'), `expected completion after approval: ${replyText(chat)}`);
    pass('ai-sdk HITL approval (await -> addToolApprovalResponse -> complete)');
  });

  console.log(
    'E2E PASS: AI SDK multi-turn + multimodal + logical File + HITL via the native Chat API.',
  );
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
