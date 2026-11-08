// The ai-sdk protocol adapter over the REAL provider path WITHOUT a live key:
// the official Vercel AI SDK client drives `real` server mode against a fake
// Anthropic upstream, so the ai-sdk encoder/request/router carry a real (wire)
// model turn end to end — the path the key-gated ai_sdk_real e2e otherwise skips.
//
// Run: (from e2e/)  node ai_sdk_via_fake_e2e.mjs

import assert from 'node:assert/strict';
import { DefaultChatTransport } from 'ai';
import { Chat } from '@ai-sdk/react';
import { modelUpstreamEnv, withServer, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const FAKE_KEY = 'sk-fake-aisdk-key'; // awaken-allow(secret):

function newChat(base, thread) {
  return new Chat({
    id: thread,
    transport: new DefaultChatTransport({ api: `${base}/v1/ai-sdk/threads/${thread}/runs` }),
  });
}

function replyText(chat) {
  return (chat.lastMessage?.parts ?? [])
    .filter((p) => p.type === 'text')
    .map((p) => p.text)
    .join('');
}

async function main() {
  const upstream = await startFakeAnthropic(FAKE_KEY, { behavior: 'reasoning' });
  try {
    await withServer('real', Number(process.env.E2E_PORT), async (base) => {
      const chat = newChat(base, 'sdk-fake');
      await chat.sendMessage({ text: 'over ai-sdk' });
      const text = replyText(chat).trim();
      /* Official-client reasoning rule: provider Thinking plus Text causes a
       * contentless bounded reasoning part and one public answer; private
       * reasoning bytes must not enter the assembled UIMessage. The Rust
       * encoder decision table owns exact phase ordering and failure edges. */
      assert.ok(text.includes('FAKE:over ai-sdk'), `ai-sdk carried the wire reply: ${JSON.stringify(text)}`);
      assert.ok(
        (chat.lastMessage?.parts ?? []).some((part) => part.type === 'reasoning'),
        `AI SDK assembled the reasoning lifecycle: ${JSON.stringify(chat.lastMessage)}`,
      );
      assert.ok(
        !JSON.stringify(chat.lastMessage).includes('PRIVATE-REASONING'),
        'private reasoning bytes never reach the AI SDK UIMessage',
      );
      assert.ok(upstream.requests.length >= 1, 'the fake upstream received the ai-sdk-driven inference call');
      pass('AI SDK client accepted private reasoning followed by the public answer');
    }, modelUpstreamEnv(upstream, { apiKey: FAKE_KEY }));
    console.log('E2E PASS: ai-sdk adapter drives the real provider path over a fake upstream.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    upstream.close();
  }
}

main();
