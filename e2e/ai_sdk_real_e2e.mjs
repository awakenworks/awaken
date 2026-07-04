// Real-model AI SDK e2e: drive awaken-server-local's ai-sdk adapter with the
// official Vercel AI SDK client (`Chat` over `DefaultChatTransport`) against the
// `real` server mode, which is backed by a live Anthropic-compatible model. Proves
// the ai-sdk protocol path carries a real model turn end to end (not the echo stub).
//
// Run: (from e2e/, with a live key)
//   ANTHROPIC_API_KEY=... ANTHROPIC_BASE_URL=... ANTHROPIC_MODEL=... node ai_sdk_real_e2e.mjs

import assert from 'node:assert/strict';
import { DefaultChatTransport } from 'ai';
import { Chat } from '@ai-sdk/react';
import { withServer, pass } from './harness.mjs';

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
  if (!process.env.ANTHROPIC_API_KEY && !process.env.KIMI_API_KEY) {
    console.log('SKIP ai_sdk_real_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  try {
    await withServer('real', 38145, async (base) => {
      const chat = newChat(base, 'sdk-real');
      await chat.sendMessage({ text: 'Reply with exactly the single word: pong' });
      const text = replyText(chat).trim();
      assert.ok(text.length > 0, `the real model returned non-empty text via ai-sdk: got ${JSON.stringify(text)}`);
      pass(`real model replied via ai-sdk: ${JSON.stringify(text.slice(0, 80))}`);
    });
    console.log('E2E PASS: ai-sdk adapter drives a real model return through the official Vercel AI SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
