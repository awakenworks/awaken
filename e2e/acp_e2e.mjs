// ACP-runtime Managed Agents e2e (R3/R4/R7): a session selects `runtime:"acp:*"`
// through the Managed API and runs on an external ACP CLI (a fake `claude --acp`
// stand-in launched as a subprocess), while a native session on the same server
// runs the built-in echo model. A second turn re-launches the ACP CLI (R7).
//
// Run: (from e2e/)  node acp_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function agentTexts(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(ev);
  }
  return events
    .filter((e) => e.type === 'agent.message')
    .map((m) => (m.content ?? []).map((c) => c.text ?? '').join('').trim());
}

async function send(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

async function main() {
  try {
    await withServer('acp', 38170, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // R3/R4: a session selecting an ACP runtime runs on the external CLI.
      const acp = await client.beta.sessions.create({
        agent: { id: 'assistant', runtime: 'acp:claude' },
        environment_id: 'env_local',
        betas: BETAS,
      });
      await send(client, acp.id, 'hello');
      let texts = await agentTexts(client, acp.id);
      assert.ok(
        texts.some((t) => t.includes('acp-runtime reply')),
        `R3/R4: acp session ran on the ACP CLI, got ${JSON.stringify(texts)}`,
      );
      pass('runtime:"acp:claude" runs on the external ACP CLI via the managed API (R3/R4)');

      // Selection: a native session on the same server runs the built-in model.
      const native = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      await send(client, native.id, 'hello');
      texts = await agentTexts(client, native.id);
      assert.ok(
        texts.some((t) => t.startsWith('Echo:')),
        `native session ran the built-in model, got ${JSON.stringify(texts)}`,
      );
      assert.ok(
        !texts.some((t) => t.includes('acp-runtime')),
        'native session must NOT hit the ACP CLI',
      );
      pass('a native session on the same server runs the built-in runtime (selection)');

      // R7: a second turn re-launches the ACP CLI (a fresh process per turn).
      await send(client, acp.id, 'again');
      texts = await agentTexts(client, acp.id);
      const acpReplies = texts.filter((t) => t.includes('acp-runtime reply')).length;
      assert.ok(acpReplies >= 2, `R7: each turn relaunches the CLI, got ${acpReplies} acp replies`);
      pass('a second turn relaunches the ACP CLI (R7)');

      // Driver-error paths reachable through the fake CLI: a malformed frame
      // and a truncated stream both surface a classified failure message. (A
      // refusal turn_end renders as a normal idle with no distinct wire signal,
      // and the provider-keyed taxonomy — auth/rate-limit/login — only arises
      // from a real CLI's output, so those stay unit-tested.)
      for (const trigger of ['acp-auth reply', 'acp-truncate reply']) {
        const s2 = await client.beta.sessions.create({
          agent: { id: 'assistant', runtime: 'acp:claude' },
          environment_id: 'env_local',
          betas: BETAS,
        });
        await send(client, s2.id, trigger);
        const texts = await agentTexts(client, s2.id);
        assert.ok(
          texts.some((t) => /the agent turn failed/i.test(t)),
          `failure "${trigger}" rendered a classified failure, got ${JSON.stringify(texts)}`,
        );
      }
      pass('ACP driver failures classify + render (malformed frame + truncation)');
    });

    console.log('E2E PASS: ACP-runtime selection + relaunch (R3/R4/R7) via the managed API.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
