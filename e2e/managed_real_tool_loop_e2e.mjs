// Real-model built-in TOOL loop (tools doc + events "requires_action" path), driven
// by a live KIMI model through the managed session. This is the core Managed Agents
// promise — "Claude autonomously runs tools" — validated end to end with a REAL model
// deciding to call a tool and awaken's sandbox actually executing it:
//
//   user.message -> agent.tool_use{bash} -> session.status_idle{requires_action}
//   -> user.tool_confirmation{allow} -> agent.tool_result -> agent.message -> idle{end_turn}
//
// The fake-upstream suites script the tool call; here the model chooses it and the
// bash runs for real, so the round-trip proves the permission gate + sandbox exec +
// result-injection path against a live provider.
//
// Gated: skips without a real key. Run: (from e2e/, with KIMI env)
//   ANTHROPIC_API_KEY=sk-kimi-... ANTHROPIC_BASE_URL=https://api.kimi.com/coding/v1/ \
//   ANTHROPIC_MODEL=kimi-k2-0711-preview node managed_real_tool_loop_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38251);
const BETAS = ['managed-agents-2026-04-01'];
const MARKER = 'BANANA';

const listEvents = async (client, id) => {
  const out = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) out.push(ev);
  return out;
};

async function main() {
  if (!process.env.ANTHROPIC_API_KEY && !process.env.KIMI_API_KEY) {
    console.log('SKIP managed_real_tool_loop_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  try {
    await withServer('real', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: `Use your bash tool to run exactly: echo ${MARKER}\nThen reply with the exact command output on its own line.` }] }],
        betas: BETAS,
      });

      // Round 1: the real model requests a tool; awaken awaits it for confirmation.
      const first = await listEvents(client, session.id);
      const toolUse = first.find((e) => e.type === 'agent.tool_use' && e.name === 'bash');
      assert.ok(toolUse, `expected a bash agent.tool_use, saw: ${[...new Set(first.map((e) => e.type))].join(', ')}`);
      assert.match(JSON.stringify(toolUse.input ?? {}), new RegExp(MARKER), 'bash command references the marker');
      const idle1 = first.filter((e) => e.type === 'session.status_idle').at(-1);
      assert.equal(idle1?.stop_reason?.type, 'requires_action', 'built-in tool awaits at requires_action');
      pass('real model requested the bash tool; session awaiting at requires_action');

      // Confirm the tool; awaken's sandbox runs it for real and feeds the result back.
      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'allow' }],
        betas: BETAS,
      });

      const all = await listEvents(client, session.id);
      const toolResult = all.find((e) => e.type === 'agent.tool_result');
      assert.ok(toolResult, 'a tool_result was committed after confirmation (sandbox executed the tool)');
      assert.match(JSON.stringify(toolResult), new RegExp(MARKER), 'the tool_result carries the real bash output');
      pass('confirmed -> sandbox executed bash -> tool_result carries the real output');

      // The real model consumes the tool output and answers with it.
      const finalMsg = all.filter((e) => e.type === 'agent.message').at(-1);
      assert.ok(finalMsg, 'a final agent.message was produced after the tool ran');
      assert.match(JSON.stringify(finalMsg.content), new RegExp(MARKER), 'final answer reflects the tool output');
      const idleLast = all.filter((e) => e.type === 'session.status_idle').at(-1);
      assert.equal(idleLast?.stop_reason?.type, 'end_turn', 'session settles at end_turn after the tool loop');
      pass('real model consumed the tool output and settled at end_turn');

      console.log('E2E PASS: real-model built-in bash tool loop (requires_action -> confirm -> sandbox exec -> answer).');
    });
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exit(1);
  }
}

main();
