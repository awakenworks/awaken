// AI SDK protocol e2e via the official Vercel `ai` package (DefaultChatTransport +
// readUIMessageStream). Covers multi-turn (the thread carries history to the
// model) and multimodal (an image file part reaches the model). Run: (from e2e/)
// npm install && node ai_sdk_e2e.mjs

import assert from 'node:assert/strict';
import { DefaultChatTransport, readUIMessageStream } from 'ai';
import { withServer, pass, RED_PNG_DATA_URI } from './harness.mjs';

/// Send one turn's message list to the thread and return the assistant reply text.
async function turn(base, thread, messages) {
  const transport = new DefaultChatTransport({
    api: `${base}/v1/ai-sdk/threads/${thread}/runs`,
  });
  const stream = await transport.sendMessages({
    chatId: thread,
    messageId: messages[messages.length - 1].id,
    trigger: 'submit-user-message',
    messages,
  });
  let text = '';
  for await (const message of readUIMessageStream({ stream })) {
    text = (message.parts ?? [])
      .filter((p) => p.type === 'text')
      .map((p) => p.text)
      .join('');
  }
  return text;
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

  console.log('E2E PASS: AI SDK multi-turn + multimodal via the `ai` package.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
