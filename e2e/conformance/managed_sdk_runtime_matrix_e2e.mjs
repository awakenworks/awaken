// Old/current SDK × Native/ACP runtime compatibility matrix.
//
// Cause/effect graph: creator SDK -> one durable Session -> operator SDK ->
// selected runtime -> committed events -> terminal lifecycle. The SDK handoff
// must not change runtime selection, pagination, event identity, or cleanup.
// Decision table: {0.105,0.117} creator × {0.117,0.105} operator ×
// {native,ACP}; every cell must create, retrieve, send, paginate, archive and
// delete through the generated Managed surface.

import assert from 'node:assert/strict';
import Anthropic0105 from '@anthropic-ai/sdk-0-105';
import Anthropic0117 from '@anthropic-ai/sdk-0-117';
import { pass, waitForSessionEventReceipt, withScenarioServer } from '../harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const CLIENTS = [
  ['0.105.0', Anthropic0105],
  ['0.117.1', Anthropic0117],
];

async function drain(items) {
  const values = [];
  for await (const item of items) values.push(item);
  return values;
}

async function exerciseCell(baseURL, runtime, creatorSpec, operatorSpec) {
  const [creatorVersion, Creator] = creatorSpec;
  const [operatorVersion, Operator] = operatorSpec;
  const creator = new Creator({ apiKey: 'e2e-dummy', baseURL });
  const operator = new Operator({ apiKey: 'e2e-dummy', baseURL });
  const agent = runtime === 'acp' ? 'acp-agent' : 'native-assistant';
  const session = await creator.beta.sessions.create({
    agent,
    environment_id: 'env_local',
    betas: BETAS,
  });
  try {
    const retrieved = await operator.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.equal(retrieved.id, session.id, `${runtime}: cross-version retrieve`);
    const receipt = await operator.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `${runtime}-${creatorVersion}-to-${operatorVersion}` }],
      }],
      betas: BETAS,
    });
    // H1: C1=operator-version exact receipt; C2=creator-version observes the
    // selected runtime reply and idle. E1=cross-version committed handoff.
    // Constraint: auto-pagination remains a separate read oracle after C2.
    // C1&&!C2=>observe; C1+C2=>E1.
    await waitForSessionEventReceipt(
      creator,
      session.id,
      receipt.data[0]?.id,
      BETAS,
      ({ delta }) => {
        const texts = delta
          .filter((event) => event.type === 'agent.message')
          .flatMap((event) => event.content ?? [])
          .map((content) => content.text ?? '');
        return texts.some(runtime === 'acp'
          ? (text) => text.includes('acp-runtime reply')
          : (text) => text.startsWith('Echo:'))
          && delta.some((event) => event.type === 'session.status_idle');
      },
      `${runtime}: cross-version exact receipt reaches selected runtime reply`,
      { pollMs: 10 },
    );
    const events = await drain(creator.beta.sessions.events.list(session.id, {
      limit: 1,
      betas: BETAS,
    }));
    assert.equal(
      new Set(events.map((event) => event.id)).size,
      events.length,
      `${runtime}: pagination must not duplicate events`,
    );
    assert.ok(events.length > 1, `${runtime}: limit=1 auto-pagination crosses pages`);
    const archived = await creator.beta.sessions.archive(session.id, { betas: BETAS });
    assert.ok(archived.archived_at, `${runtime}: archive`);
    const deleted = await operator.beta.sessions.delete(session.id, { betas: BETAS });
    assert.equal(deleted.type, 'session_deleted', `${runtime}: delete`);
    pass(`${runtime} Session handoff ${creatorVersion} -> ${operatorVersion}`);
  } catch (error) {
    try { await creator.beta.sessions.delete(session.id, { betas: BETAS }); } catch {}
    throw error;
  }
}

await withScenarioServer('acp', 'echo', 38187, async (baseURL) => {
  for (const runtime of ['native', 'acp']) {
    await exerciseCell(baseURL, runtime, CLIENTS[0], CLIENTS[1]);
    await exerciseCell(baseURL, runtime, CLIENTS[1], CLIENTS[0]);
  }
});

console.log('E2E PASS: SDK 0.105/0.117 handoffs preserve Native and ACP Session behavior.');
