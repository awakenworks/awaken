// ADR-0052 assistant RUN e2e: the seeded management assistant actually runs and
// invokes all six management tools end to end. The `config` server (with
// the assistant seeded + the admin executables wired) is driven by the fake upstream
// reproducing the `adminDrive` scenario, which sequences the six tool calls. This
// exercises the tool *executables* + the audit sink over a real managed session,
// which the config-plane HTTP e2e cannot reach.
//
//   capabilities → draft → patch → validate → environment → explain → final summary
//
// Run: (from e2e/)  node adr0052_admin_run_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass, waitForSessionEventReceipt } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38295);
const BETAS = ['managed-agents-2026-04-01'];
const ASSISTANT = '__admin_assistant';

async function main() {
  await withScenarioServer('config', 'adminDrive', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    // Run the seeded assistant as an ordinary agent over the managed protocol.
    const session = await client.beta.sessions.create({
      agent: ASSISTANT,
      environment_id: 'env_local',
      betas: BETAS,
    });
    // C1=exact Admin User receipt; C2=all six tool effects and final marker.
    // E1=C2 after C1. K: the ordinary Agent transcript is the only execution
    // proof. Decision A1 C1&&!C2=>retry; A2 C1+C2=>assert all tools.
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'help me author an agent' }] }],
      betas: BETAS,
    });

    const receiptId = receipt.data[0]?.id;
    assert.equal(typeof receiptId, 'string', 'A1 exact Admin User Event receipt');
    const { delta: events } = await waitForSessionEventReceipt(
      client,
      session.id,
      receiptId,
      BETAS,
      ({ delta }) => JSON.stringify(delta).includes('ADMIN-RUN-DONE')
        && [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn',
      'A1 Admin Assistant Run to commit all tool effects',
    );
    const blob = JSON.stringify(events);

    // Every admin tool ran (their ids appear as tool calls / results in the transcript).
    for (const id of [
      'admin_get_platform_capabilities',
      'admin_draft_agent',
      'admin_patch_agent',
      'admin_validate_agent',
      'admin_draft_environment',
      'admin_explain_console',
    ]) {
      assert.ok(blob.includes(id), `assistant invoked ${id} end to end`);
    }
    // The run reached its natural end after all six tools executed successfully:
    // the driving model only emits this marker after it has seen six tool RESULTS,
    // so reaching it proves each admin executable ran and returned (not just emitted).
    const assistantText = events
      .filter((e) => e.type === 'agent.message')
      .map((m) => JSON.stringify(m.content))
      .join('');
    assert.ok(assistantText.includes('ADMIN-RUN-DONE'), 'assistant finished after driving all six tools');

    pass('management assistant ran and invoked all six admin tools end to end');
  });

  console.log('\nE2E PASS: ADR-0052 assistant executes its six management tools live.');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
