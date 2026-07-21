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
import { withScenarioServer, pass } from './harness.mjs';

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
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'help me author an agent' }] }],
      betas: BETAS,
    });

    const events = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
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
