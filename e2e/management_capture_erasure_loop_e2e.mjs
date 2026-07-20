// Full ADR-0050 loop end-to-end: a real agent turn with content capture = full
// writes the prompt/completion into a subject-tagged store; GDPR erasure then
// removes exactly that subject's captured content. Drives a real ai-sdk turn
// (echo model over the real runtime) then hits the Awaken erasure endpoint.
//
// Run: (from e2e/)  node management_capture_erasure_loop_e2e.mjs

import assert from 'node:assert/strict';
import { DefaultChatTransport } from 'ai';
import { Chat } from '@ai-sdk/react';
import { withRealServer, pass } from './harness.mjs';

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
  await withRealServer(
    'echo',
    38194,
    async (base) => {
      // Run a real turn. With AWAKEN_CONTENT_CAPTURE=full + AWAKEN_CONTENT_SUBJECT
      // set, the engine writes the prompt + completion into the subject-tagged
      // captured-content store (the process-global sink).
      const chat = newChat(base, 'cap-loop');
      await chat.sendMessage({ text: 'please capture this content' });
      assert.ok(
        replyText(chat).includes('please capture this content'),
        `turn did not complete: ${replyText(chat)}`,
      );
      pass('ran a real turn with content capture=full (subject dsub_full)');

      // Erasure removes exactly this subject's captured content.
      const res = await fetch(`${base}/v1/user_profiles/dsub_full/erasure`, { method: 'POST' });
      assert.equal(res.status, 200, `erasure status ${res.status}`);
      const body = await res.json();
      assert.ok(
        body.records_removed > 0,
        `expected captured content to be erased, got ${body.records_removed}`,
      );
      pass(`run→capture→store→erase: ${body.records_removed} captured records erased`);

      // A retry returns the same durable receipt. `records_removed` is cumulative
      // accountability evidence, not the delta of this HTTP attempt.
      const again = await (
        await fetch(`${base}/v1/user_profiles/dsub_full/erasure`, { method: 'POST' })
      ).json();
      assert.equal(
        again.records_removed,
        body.records_removed,
        'an idempotent retry returns the original durable erasure receipt',
      );
      pass('erasure is idempotent — retry returned the same accountability receipt');
    },
    { extraEnv: { AWAKEN_CONTENT_CAPTURE: 'full', AWAKEN_CONTENT_SUBJECT: 'dsub_full' } },
  );
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
