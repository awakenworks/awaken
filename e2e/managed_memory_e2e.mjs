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
  waitForValue,
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
  await withScenarioServer('memory', 'memory', Number(process.env.E2E_PORT), async (baseUrl, upstream) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const store = await scenarioMemoryStore(client, MEMORY_HEADERS);
    // Read the canonical committed Memory content; HTTP/decode errors propagate.
    const memories = async () => (await client.get(
      `/v1/memory_stores/${store.id}/memories?view=full`, {headers:MEMORY_HEADERS},
    )).data;
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
    const extractionStart = upstream.requests.length;
    const a = await session();
    const first = await turn(client, a.id, 'remember the sky');
    assert.ok(first.includes('echo:remember the sky'), `probe echoes the turn: ${first}`);
    // M1 separates terminal parent reply from asynchronous extraction. The same
    // provider ledger must observe an extractor role, and the canonical Store
    // must acknowledge the fact before later-session recall is asserted.
    const sky = await waitForValue(memories,
      (entries) => entries.some((entry) => entry.content?.includes('sky is green')),
      'M1 extractor content reaches the canonical MemoryStore', {timeoutMs:10_000,pollMs:100});
    assert.ok(sky.some((entry) => entry.content?.includes('sky is green')), 'M1 committed extracted sky content');
    assert.ok(upstream.requests.slice(extractionStart).some((request) => request.memoryExtractor),
      'M1 the real extractor phase reached the shared provider fixture');
    pass('session A terminal and extractor content acknowledged by the canonical Store');

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
    // M2 grows actual Store facts, not just completed parent turns. Reuse the
    // original request ledger to witness the selector phase and its >12-row
    // native manifest; text containing fact- alone is not selector evidence.
    const grown = await waitForValue(memories,
      (entries) => entries.filter((entry) => entry.content?.includes('remember fact-')).length > 12,
      'M2 more than twelve extracted facts are committed', {timeoutMs:10_000,pollMs:100});
    assert.ok(grown.filter((entry) => entry.content?.includes('remember fact-')).length > 12,
      'M2 canonical Store is beyond the selector threshold');
    const selectionStart = upstream.requests.length;
    const selectorPrefix = "You select which of a user's saved memories are relevant to their current message.";
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
    const selectorRequests = upstream.requests.slice(selectionStart)
      .filter((request) => request.system.startsWith(selectorPrefix));
    assert.ok(selectorRequests.length > 0, 'M2 the default selector phase was actually invoked');
    assert.ok(selectorRequests.some((request) => request.lastUserText.includes('Saved memories:\n')
      && request.lastUserText.includes('[12] ')), 'M2 selector received a native manifest beyond twelve entries');
    pass('grown-store fact recalled after an observed selector request with >12 manifest entries');
  });
  console.log('E2E PASS: out-of-band memory extraction + bounded recall across sessions.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
