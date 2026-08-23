// Skills end-to-end (ADR-0036) via the Anthropic TS SDK.
//
// The `skills` server offers two configured skills to a filesystem-capable
// Managed Session. Runtime freezes and projects both as Anthropic-compatible
// SKILL.md files, keeps only metadata/path in the prompt, and reads one on demand.
//
// Run: (from e2e/)  node managed_skills_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  spawnServer,
  stopServer,
  waitForPort,
  pass,
  startUpstream,
  realServerEnv,
  waitForSessionEventReceipt,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38183);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

async function main() {
  let upstream;
  let server;
  try {
    // Startup/cleanup decision table: C1 upstream starts, C2 product child
    // starts and becomes ready. S1 C1+C2 => run and close both; S2 C1+!C2 =>
    // close upstream even though no request was served; S3 !C1 => no owned
    // handle exists. This keeps a readiness failure terminal instead of leaving
    // the Node process alive on the upstream listener.
    upstream = await startUpstream('skills');
    ({ server } = spawnServer('skills', PORT, realServerEnv('skills', upstream, { mode: 'skills' })));
    await waitForPort(PORT);
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    // Cause/effect rules: C1 exact User receipt; C2 filesystem-capable Managed
    // Session; C3 frozen configured Skills. S1 C1+C2+C3 => one `read`, SKILL.md
    // body in its result, reply uses it, and no semantic activation calls.
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'please discover and use a skill' }] }],
      betas: BETAS,
    });
    const receiptId = receipt.data[0]?.id;
    assert.equal(typeof receiptId, 'string', 'K1 exact Skill Run User Event receipt');
    const { delta: events } = await waitForSessionEventReceipt(
      client,
      session.id,
      receiptId,
      BETAS,
      ({ delta }) => delta.filter((event) => event.type === 'agent.tool_use').length >= 1
        && delta.some((event) => event.type === 'agent.message')
        && delta.some((event) => event.type === 'session.status_idle'),
      'K1 Skill Run to commit discover, read, and reply effects',
    );

    // Anthropic-compatible filesystem delivery has exactly one activation path.
    const toolNames = events.filter((e) => e.type === 'agent.tool_use').map((e) => e.name);
    assert.deepEqual(toolNames, ['read'], 'the model loaded only the advertised SKILL.md');
    assert.ok(!toolNames.includes('list_skills') && !toolNames.includes('Skill'));
    pass('filesystem metadata discovery → on-demand SKILL.md read used one path');

    // The on-demand read, rather than the system prompt, delivered the body.
    const results = JSON.stringify(events.filter((e) => e.type === 'agent.tool_result').map((r) => r.content));
    assert.ok(results.includes('GREETING-FROM-SKILL'), 'SKILL.md body reached the read result');
    pass('the selected SKILL.md body was delivered on demand');

    // The reply reflects the activated skill's instructions.
    const reply = JSON.stringify(events.filter((e) => e.type === 'agent.message').map((m) => m.content));
    assert.ok(reply.includes('GREETING-FROM-SKILL'), "the run used the activated skill's instructions");
    pass('the loaded skill instructions reached the reply (discover → read → use)');

    console.log('E2E PASS: filesystem Skill discovery → read → use via TS SDK (ADR-0036).');
  } finally {
    if (server) await stopServer(server);
    upstream?.close();
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
