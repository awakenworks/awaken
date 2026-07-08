// Managed protocol multimodal e2e via the official @anthropic-ai/sdk: an image
// content block posted on a session reaches the model. Run: (from e2e/)
// npm install && node managed_multimodal_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass, RED_PNG_B64 } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function lastAgentText(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  const msg = [...events].reverse().find((e) => e.type === 'agent.message');
  return (msg?.content ?? [])
    .filter((b) => b.type === 'text')
    .map((b) => b.text)
    .join('');
}

async function main() {
  await withRealServer('vision', 38160, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
    await client.beta.sessions.events.send(session.id, {
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
    const reply = await lastAgentText(client, session.id);
    assert.ok(reply.includes('image/png'), `image did not reach the model: ${reply}`);
    pass('managed multimodal (image reached the model)');
  });

  console.log('E2E PASS: Managed multimodal via @anthropic-ai/sdk.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
