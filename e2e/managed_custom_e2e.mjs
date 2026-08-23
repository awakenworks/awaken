// Custom (client-executed) tool end-to-end with the official Anthropic TS SDK:
// the model calls a client tool -> the Run parks as `agent.custom_tool_use` +
// `requires_action`; the client executes it and returns
// `user.custom_tool_result`; the Run resumes and the model uses the result.
//
// Uses the custom server (AWAKEN_MODEL_MODE=custom): a `submit_answer` client
// tool (model-visible, no server-side executable).
//
// Run: (from e2e/)  npm install && node managed_custom_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { waitForSessionEventReceipt, withScenarioServer } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38104);
const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  // The `custom` host config (the `submit_answer` client tool) with the model on the
  // real wire (the `custom` behavior: call the tool, then reply with its result).
  await withScenarioServer('custom', 'custom', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

    // Cause/effect graph: C1=a User input makes the published custom tool
    // pending; C2=the reply uses that qualified Event id and the custom-result
    // family; C3=the result contains "42". Effects: E1=the Run parks with one
    // requires_action owning C1; E2=the retained result resumes that Run once;
    // E3=the Provider observes C3 and the Run reaches end_turn. Decision rule
    // C1 is M1(C1)->E1; the continuation is M2(C1+C2+C3)->E2+E3. Family/id
    // rejection combinations are owned by managed_error_paths_e2e.mjs and the
    // protocol decision table, so this official-SDK case owns the positive path.
    // Constraints/invariant: the qualified public custom-tool Event id is the
    // only continuation authority and the retained Run resumes at most once.
    const initialReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'solve it' }] }],
      betas: BETAS,
    });
    const initialReceiptId = initialReceipt.data[0]?.id;
    assert.equal(typeof initialReceiptId, 'string', 'M1 returns its exact User Event receipt');
    let { events } = await waitForSessionEventReceipt(
      client,
      session.id,
      initialReceiptId,
      BETAS,
      ({ delta }) => {
        const pending = delta.find((event) => event.type === 'agent.custom_tool_use');
        const idle = [...delta].reverse().find((event) => event.type === 'session.status_idle');
        return pending
          && idle?.stop_reason?.type === 'requires_action'
          && idle.stop_reason.event_ids.includes(pending.id);
      },
      'M1 custom tool use to become durably pending',
    );
    const customUse = events.find((e) => e.type === 'agent.custom_tool_use');
    assert.ok(customUse, `expected agent.custom_tool_use, got: ${events.map((e) => e.type)}`);
    assert.equal(customUse.name, 'submit_answer');
    const idle = events.find((e) => e.type === 'session.status_idle');
    assert.equal(idle.stop_reason.type, 'requires_action');
    assert.ok(idle.stop_reason.event_ids.includes(customUse.id));

    const resultReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.custom_tool_result', custom_tool_use_id: customUse.id, content: [{ type: 'text', text: '42' }] }],
      betas: BETAS,
    });
    assert.equal(resultReceipt.data[0]?.type, 'user.custom_tool_result', 'M2 retains the typed SDK reply');
    const resultReceiptId = resultReceipt.data[0]?.id;
    assert.equal(typeof resultReceiptId, 'string', 'M2 returns its exact custom-result receipt');
    ({ events } = await waitForSessionEventReceipt(
      client,
      session.id,
      resultReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.message' && event.content?.[0]?.text?.includes('42'))
        && [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn',
      'M2 custom result to commit and resume the parked Run',
    ));
    const messages = events.filter((e) => e.type === 'agent.message').map((e) => e.content[0].text);
    assert.ok(messages.some((m) => m.includes('42')), `client result reached the model: ${messages}`);
    const lastIdle = [...events].reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(lastIdle.stop_reason.type, 'end_turn');

    console.log('E2E PASS: custom (client-executed) tool round-trip via TS SDK.');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
