// Live-LLM proof for official Managed SDK File image input.
//
// Cause/effect rule L1: configured live Anthropic-compatible endpoint + uploaded
// red PNG File + official SDK image(file_id) -> Files-authorized immutable
// materialization -> Anthropic Messages image(base64) -> answer `RED`.
// Constraint: the prompt does not state the image color; observing `RED` proves
// the real model inspected materialized bytes rather than echoing input text.
// FMECA: stale/missing catalog row, digest/MIME drift, leaked logical file_id, or
// unsupported provider document mapping (all critical/high) terminate without a
// false-positive marker. This test is intentionally live and skipped only when no
// Anthropic-compatible credential is configured.

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
// @ts-ignore -- shared JS harness deliberately serves both JS and TS scenarios.
import { pass, withServer } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  if (!process.env.ANTHROPIC_API_KEY && !process.env.KIMI_API_KEY) {
    console.log('SKIP managed_content_blocks_real_e2e: no live Anthropic-compatible key.');
    return;
  }
  // Fetch a conventionally encoded PNG because several Anthropic-compatible
  // providers reject valid but aggressively optimized tiny PNG encodings.
  const fixtureResponse = await fetch('https://placehold.co/256x256/FF0000/FF0000.png');
  assert.equal(fixtureResponse.status, 200, 'load live red PNG fixture');
  const fixture = Buffer.from(await fixtureResponse.arrayBuffer());
  await withServer('real', 38338, async (baseUrl: string) => {
    const client = new Anthropic({ apiKey: 'e2e-local-only', baseURL: baseUrl }); // awaken-allow: secret
    const file = await client.beta.files.upload({
      file: await toFile(fixture, 'live-red.png', {
        type: 'image/png',
      }),
      betas: BETAS,
    });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [
          { type: 'image', source: { type: 'file', file_id: file.id } },
          { type: 'text', text: 'Reply with exactly the uppercase English name of the predominant image color.' },
        ],
      }],
      betas: BETAS,
    });
    const events = [];
    for await (const event of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
      events.push(event);
    }
    const answer = events
      .filter((event) => event.type === 'agent.message')
      .flatMap((event) => event.content)
      .filter((block) => block.type === 'text')
      .map((block) => block.text)
      .join('\n');
    const diagnostics = events.map((event) => event.type === 'session.error'
      ? { type: event.type, error: event.error }
      : { type: event.type });
    assert.match(
      answer,
      /\bRED\b/u,
      `L1 live answer: ${JSON.stringify(answer)}; events=${JSON.stringify(diagnostics)}`,
    );
    pass('live Anthropic-compatible model inspected an SDK image File through Awaken materialization');
  });
  console.log('E2E PASS: real LLM validated Managed SDK File image content.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
