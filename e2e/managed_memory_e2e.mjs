// Out-of-band memory e2e: a turn in session A triggers the background
// extractor sub-run (which saves a memory via the `write_memory` tool); a LATER
// session B sees that memory injected request-only by the recall plugin. The
// `memory` mode's probe model surfaces the injected context in its reply, so
// the whole loop — extract → store → recall → inject — is observable on the wire.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  pass,
  scenarioMemoryStore,
  waitForSessionEventReceipt,
  withScenarioServer,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };

async function turn(client, sessionId, text) {
  // C1=exact User receipt; C2=memory-aware reply+terminal. E1=post-C1 reply.
  // K: background extraction/recall remains independently durable. Decision
  // M1 C1&&!C2=>retry; M2 C1+C2=>return this Run's reply.
  const receipt = await client.beta.sessions.events.send(sessionId, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'M1 exact memory Run User Event receipt');
  const { delta } = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta: later }) => later.some((event) => event.type === 'agent.message')
      && later.some((event) => event.type === 'session.status_idle'),
    `M1 memory Run for ${JSON.stringify(text)} to commit`,
  );
  return delta
    .filter((event) => event.type === 'agent.message')
    .map((event) => event.content.map((block) => block.text ?? '').join(''))
    .join('\n');
}

async function main() {
  await withScenarioServer('memory', 'memory', 38197, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const store = await scenarioMemoryStore(client, MEMORY_HEADERS);
    const session = () => client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
      resources: [{ type: 'memory_store', memory_store_id: store.id }],
    });

    // Cause-effect graph / decision table:
    // M1 published memory plugin + official id-only read-write Store binding ->
    // the catalog derives the mount, extraction runs at terminal, and later
    // recall injects the fact; M2 the same binding with >12 facts -> selector
    // bounds the recall; M3 a plain non-Memory mount without the published
    // binding -> mount only, no automatic memory effect (Host A1). This e2e
    // drives M1/M2 without creating a client-side path authority.
    const a = await session();
    const first = await turn(client, a.id, 'remember the sky');
    assert.ok(first.includes('echo:remember the sky'), `probe echoes the turn: ${first}`);
    pass('session A turn completed (extractor fired in the background)');

    // Session B (later): the recall plugin injects the stored memory into the
    // request; the probe model surfaces it. The extractor is fire-and-forget,
    // so poll briefly.
    let recalled = '';
    for (let i = 0; i < 20; i += 1) {
      await new Promise((r) => setTimeout(r, 500));
      const b = await session();
      recalled = await turn(client, b.id, 'what color is the sky?');
      if (recalled.includes('sky is green')) break;
    }
    assert.ok(
      recalled.includes('sky is green'),
      `a later session sees the extracted memory in its injected context: ${recalled}`,
    );
    pass('extract -> store -> recall -> inject observable across sessions');

    // Accumulate enough distinct memories to cross the recall SELECTOR
    // threshold (default 12): each session saves a `fact-<n>` memory, then a
    // later recall runs the relevance selector over the grown store.
    for (let n = 0; n < 14; n += 1) {
      const s = await session();
      await turn(client, s.id, `remember fact-${n}`);
    }
    let selected = '';
    for (let i = 0; i < 20; i += 1) {
      await new Promise((r) => setTimeout(r, 500));
      const b = await session();
      selected = await turn(client, b.id, 'what facts do you recall about fact-13?');
      if (selected.includes('fact-')) break;
    }
    assert.ok(
      selected.includes('fact-'),
      `the relevance selector injected a memory from the grown store: ${selected}`,
    );
    pass('recall selector fires once the store passes its threshold (>12 memories)');
  });
  console.log('E2E PASS: out-of-band memory extraction + bounded recall across sessions.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
