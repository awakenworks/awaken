// AI SDK HITL DENY path e2e (the gap the approve-only ai_sdk_e2e leaves open).
// A mutating tool parks (state `input-available`); the client answers with the
// AI SDK `output-denied` decision instead of a result. The real server decodes it
// to a Cancel resume, the tool is NOT run, and the run still completes.
//
// The approve path (ai_sdk_e2e.mjs) drives the official `Chat` sugar; the deny
// STATE (`output-denied`) is a wire decision the happy-path client does not
// emit, so this test posts the resume request over the same real HTTP endpoint
// and reads the UI Message Stream (SSE) back. Real server, real router/decoder,
// real runtime — only the client sugar is dropped for the one unsupported state.
//
// Run: (from e2e/)  npm install && node ai_sdk_hitl_deny_e2e.mjs

import assert from 'node:assert/strict';
import { DefaultChatTransport } from 'ai';
import { Chat } from '@ai-sdk/react';
import { withRealServer, pass } from './harness.mjs';

const THREAD = 'sdk-hitl-deny';
const NOTE = 'DENY-ME-NOTE';

function runsUrl(base, thread) {
  return `${base}/v1/ai-sdk/threads/${thread}/runs`;
}

// Read a UI Message Stream (SSE) response body, accumulating text-delta parts
// into the assistant's reply text.
async function readStreamText(res) {
  const raw = await res.text();
  let text = '';
  for (const line of raw.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed.startsWith('data:')) continue;
    const payload = trimmed.slice(5).trim();
    if (!payload || payload === '[DONE]') continue;
    let ev;
    try {
      ev = JSON.parse(payload);
    } catch {
      continue;
    }
    if (ev.type === 'text-delta' && typeof ev.delta === 'string') text += ev.delta;
    if (ev.type === 'text' && typeof ev.text === 'string') text += ev.text;
  }
  return text;
}

async function main() {
  await withRealServer('probe', 38147, async (base) => {
    // Turn 1: drive the real park through the official Chat client.
    const chat = new Chat({
      id: THREAD,
      transport: new DefaultChatTransport({ api: runsUrl(base, THREAD) }),
    });
    await chat.sendMessage({ text: NOTE });
    const toolPart = (chat.lastMessage?.parts ?? []).find((p) => p.toolCallId);
    assert.ok(toolPart, `expected a parked tool part: ${JSON.stringify(chat.lastMessage?.parts)}`);
    assert.equal(toolPart.state, 'input-available', 'the mutating tool should await a decision');
    pass('ai-sdk deny: mutating tool parked (input-available)');

    // Turn 2: answer with `output-denied` over the same real endpoint. The
    // assistant tool part carries the decision; no user/system content, so the
    // server treats it as a resume-only request.
    const res = await fetch(runsUrl(base, THREAD), {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: THREAD,
        messages: [
          {
            id: 'a-deny',
            role: 'assistant',
            parts: [
              {
                type: toolPart.type,
                toolCallId: toolPart.toolCallId,
                state: 'output-denied',
              },
            ],
          },
        ],
      }),
    });
    assert.equal(res.status, 200, `resume POST should succeed, got ${res.status}`);
    const text = await readStreamText(res);

    // The run resumed and reached a terminal turn even though the tool was denied.
    assert.ok(text.includes('done'), `expected completion after deny, got: ${JSON.stringify(text)}`);
    // The write was blocked, so nothing echoes the note back.
    assert.ok(!text.includes(NOTE), `deny must block the write; leaked note in: ${JSON.stringify(text)}`);
    pass('ai-sdk deny: tool blocked, run still completes (park -> output-denied -> complete)');
  });

  console.log('E2E PASS: AI SDK HITL deny round-trip (output-denied) via the real endpoint.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
