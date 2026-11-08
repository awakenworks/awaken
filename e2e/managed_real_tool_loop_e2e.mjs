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
// Gated: skips without a real key. Run: (from e2e/, with a live key in the
// controller environment or ~/.bashrc; it reaches the server only through the
// config-plane API)  node managed_real_tool_loop_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  managedAgentWithAlwaysAskTools,
  pass,
  waitForSessionEventReceipt,
  withServer,
  modelUpstreamEnv,
} from './harness.mjs';
import { liveAnthropicCompatibleUpstream } from './model_upstream.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38251);
const BETAS = ['managed-agents-2026-04-01'];
const MARKER = 'BANANA';

async function main() {
  const live = liveAnthropicCompatibleUpstream();
  if (!live) {
    console.log('SKIP managed_real_tool_loop_e2e: no live Anthropic-compatible key (ANTHROPIC_API_KEY or ~/.bashrc).');
    return;
  }
  try {
    await withServer('real', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const session = await client.beta.sessions.create({
        agent: managedAgentWithAlwaysAskTools(['bash']),
        environment_id: 'env_local',
        betas: BETAS,
      });
      // Tool-loop decision T1: C0 the Session owns bash=always_ask; C1 exact
      // prompt receipt and C2 model requests
      // bash; E1 processed receipt, matching tool_use, and requires_action. K1
      // older tool calls cannot satisfy this turn. D1=C1+C2=>E1.
      const promptReceipt = (await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: `Use your bash tool to run exactly: echo ${MARKER}\nThen reply with the exact command output on its own line.` }] }],
        betas: BETAS,
      })).data[0];

      // Round 1: the real model requests a tool; awaken awaits it for confirmation.
      const { delta: first } = await waitForSessionEventReceipt(
        client,
        session.id,
        promptReceipt.id,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.tool_use' && event.name === 'bash')
          && delta.some((event) => event.type === 'session.status_idle'
            && event.stop_reason?.type === 'requires_action'),
        'real model bash request and requires_action boundary',
        { timeoutMs: 180_000 },
      );
      const toolUse = first.find((e) => e.type === 'agent.tool_use' && e.name === 'bash');
      assert.ok(toolUse, `expected a bash agent.tool_use, saw: ${[...new Set(first.map((e) => e.type))].join(', ')}`);
      assert.match(JSON.stringify(toolUse.input ?? {}), new RegExp(MARKER), 'bash command references the marker');
      const idle1 = first.filter((e) => e.type === 'session.status_idle').at(-1);
      assert.equal(idle1?.stop_reason?.type, 'requires_action', 'built-in tool awaits at requires_action');
      pass('real model requested the bash tool; session awaiting at requires_action');

      // Confirm the tool; awaken's sandbox runs it for real and feeds the result back.
      // Confirmation decision T2: C3 exact allow receipt; E2 processed receipt,
      // sandbox tool_result, final marker answer, and end_turn. K2 events before
      // the confirmation receipt cannot satisfy E2. D2=T1+C3=>E2.
      const confirmationReceipt = (await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'allow' }],
        betas: BETAS,
      })).data[0];

      const { delta: all } = await waitForSessionEventReceipt(
        client,
        session.id,
        confirmationReceipt.id,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.tool_result')
          && delta.some((event) => event.type === 'agent.message')
          && delta.some((event) => event.type === 'session.status_idle'
            && event.stop_reason?.type === 'end_turn'),
        'confirmed bash result and final answer',
        { timeoutMs: 180_000 },
      );
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
    }, modelUpstreamEnv(null, live));
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exit(1);
  }
}

main();
