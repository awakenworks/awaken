// The management assistant is an ordinary published Agent. Drive its
// `admin_draft_agent` tool through the real managed protocol to prove flexible
// SDK-shaped MCP/Skill/Multiagent input exists only at the tool adapter and is
// normalized into the typed Agent aggregate before persistence/validation.
// Cause graph: typed MCP/Skill/delegation inputs -> deserialize -> Config
// validation/persistence. A malformed edge stops before persistence.
//
// | Rule | MCP | Skill | roster | Result |
// |---|---|---|---|---|
// | T1 | complete typed binding | string + `{id}` | coordinator | persist |
// | T2 | scalar | - | - | reject |
// | T3 | malformed credential ref | - | - | reject |
// | T4 | - | object without `id` | - | reject |
// | T5 | - | - | unsupported type | reject |

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, waitForSessionEventReceipt } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 39415);
const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  await withScenarioServer('config', 'adminTypedEdges', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const session = await client.beta.sessions.create({
      agent: '__admin_assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    // C1=exact Admin User receipt; C2=typed-edge final marker+terminal. E1=C2
    // after C1. K: malformed tool inputs remain scenario-owned transcript
    // effects. Decision T1 C1&&!C2=>retry; T2 C1+C2=>assert all edge rows.
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: 'exercise typed authoring boundaries' }],
      }],
      betas: BETAS,
    });

    const receiptId = receipt.data[0]?.id;
    assert.equal(typeof receiptId, 'string', 'T1 exact typed-edge User Event receipt');
    const { delta: events } = await waitForSessionEventReceipt(
      client,
      session.id,
      receiptId,
      BETAS,
      ({ delta }) => JSON.stringify(delta).includes('ADMIN-TYPED-EDGES-DONE')
        && delta.some((event) => event.type === 'session.status_idle'),
      'T1 typed-edge Admin Run to commit its decision rows',
    );
    const transcript = JSON.stringify(events);
    assert.ok(transcript.includes('ADMIN-TYPED-EDGES-DONE'));
    assert.ok(transcript.includes('expected struct AgentMcpServerBinding'));
    assert.ok(transcript.includes('invalid type: integer'));
    assert.ok(transcript.includes('data did not match any variant of untagged enum AgentSkillBindingInput'));
    assert.ok(transcript.includes('multiagent.type must be `coordinator`'));
    assert.equal(
      (transcript.match(/admin_draft_agent/g) ?? []).length >= 5,
      true,
      'all typed and rejected inputs crossed the same ordinary Agent tool boundary',
    );

    console.log(
      'ADMIN TYPED AUTHORING TS E2E PASS: SDK unions normalize once and malformed MCP, Skill, and delegation values fail before Agent persistence.',
    );
  });
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
