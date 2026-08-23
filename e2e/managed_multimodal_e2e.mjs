// Managed protocol multimodal e2e via the official @anthropic-ai/sdk: an image
// content block posted on a session reaches the model. Run: (from e2e/)
// npm install && node managed_multimodal_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass, RED_PNG_B64, waitForSessionEventReceipt } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

function lastAgentText(events) {
  const msg = [...events].reverse().find((e) => e.type === 'agent.message');
  return (msg?.content ?? [])
    .filter((b) => b.type === 'text')
    .map((b) => b.text)
    .join('');
}

async function main() {
  await withRealServer('vision', 38160, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    // C1=exact multimodal User receipt; C2=image-aware reply+terminal. E1=C2
    // after C1. K: the official content block owns the image bytes. Decision
    // V1 C1&&!C2=>retry; V2 C1+C2=>assert media proof.
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [
        {
          type: 'user.message',
          content: [
            { type: 'image', source: { type: 'base64', media_type: 'image/png', data: RED_PNG_B64 } },
            { type: 'text', text: 'what color is this' },
          ],
        },
      ],
      betas: BETAS,
    });
    const receiptId = receipt.data[0]?.id;
    assert.equal(typeof receiptId, 'string', 'V1 exact multimodal User Event receipt');
    const { delta } = await waitForSessionEventReceipt(
      client,
      session.id,
      receiptId,
      BETAS,
      ({ delta: later }) => later.some((event) => event.type === 'agent.message')
        && later.some((event) => event.type === 'session.status_idle'),
      'V1 multimodal Run to commit its image-aware reply',
    );
    const reply = lastAgentText(delta);
    assert.ok(reply.includes('image/png'), `image did not reach the model: ${reply}`);
    pass('managed multimodal (image reached the model)');
  });

  console.log('E2E PASS: Managed multimodal via @anthropic-ai/sdk.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
