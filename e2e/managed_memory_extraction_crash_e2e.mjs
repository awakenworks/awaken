// Crash recovery for the durable Memory extraction state machine.
//
// The fake upstream delays every response and counts a request as soon as it
// arrives. The test kills server A after the extractor request arrived but before
// its response, restarts over the same storage directory, rehydrates the original
// Session, and proves the same intent retains an unknown outcome without unsafe
// replay. Successful durable/stage recovery remains owned by the adjacent E2Es.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { extractionRows } from './fixtures/memory_extraction_fixture.mjs';
import {
  cleanupFixtureTree,
  pass,
  realServerEnv,
  scenarioMemoryStore,
  spawnServer,
  startUpstream,
  stopServer,
  hardKillServer as hardKill,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38243);
const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const STORE_DIR = `/tmp/awaken-mem-extract-crash-e2e-${process.pid}`;
const MARKER = 'fact-otter9crash';

let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

async function reply(sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return events
    .filter((event) => event.type === 'agent.message')
    .map((event) => event.content.map((block) => block.text ?? '').join(''))
    .join('\n');
}

async function turn(sessionId, text) {
  // C1=exact User receipt; C2=reply+terminal before crash selection. E1=the
  // post-C1 delta proves the root Run committed. K: extractor crash/recovery is
  // observed separately. Decision C1&&!C2=>retry; C1+C2=>return committed reply.
  const receipt = await client.beta.sessions.events.send(sessionId, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'exact crash-extraction User Event receipt');
  const { delta } = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta: later }) => later.some((event) => event.type === 'agent.message')
      && later.some((event) => event.type === 'session.status_idle'),
    `crash-extraction Run for ${JSON.stringify(text)} to commit`,
  );
  return delta
    .filter((event) => event.type === 'agent.message')
    .map((event) => event.content.map((block) => block.text ?? '').join(''))
    .join('\n');
}

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const servers = [];
  const upstream = await startUpstream('memory', { delayMs: 2_000 });
  try {
    const env = {
      SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR,
      ...realServerEnv('memory', upstream, { mode: 'memory' }),
    };
    const a = spawnServer('memory', PORT, env);
    servers.push(a.server);
    await waitForPort(PORT);

    const store = await scenarioMemoryStore(client, MEMORY_HEADERS);
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
      resources: [{ type: 'memory_store', memory_store_id: store.id }],
    });
    assert.ok((await turn(session.id, `remember ${MARKER}`)).includes(MARKER), 'terminal turn committed');

    await waitForValue(
      () => upstream.received >= 2 && upstream.received > upstream.requests.length,
      Boolean,
      'extractor inference never became observably in flight',
    );
    const received = upstream.received;
    const database = path.join(STORE_DIR, 'sessions.db');
    const originals = extractionRows(database);
    assert.equal(originals.length, 1, 'U1 exactly one real extraction before the crash');
    const [original] = originals;
    assert.equal(original.status, 'claimed', 'the crash cuts a real claimed extraction');
    // Reject missing coordinates before comparing restart identity: two absent
    // fields must never satisfy U1's stable Run/Thread oracle.
    for (const coordinate of ['intent_id', 'auxiliary_run_id', 'auxiliary_thread_id']) {
      assert.equal(typeof original.intent[coordinate], 'string', `U1 ${coordinate} exists`);
      assert.ok(original.intent[coordinate].trim(), `U1 ${coordinate} is nonempty`);
    }
    await hardKill(a.server);
    servers.pop();
    pass('server A crashed while durable extraction inference was in flight');

    const b = spawnServer('memory', PORT, env);
    servers.push(b.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

    // Cause/effect decision table (actual process loss, no persisted-row edits):
    // C1 upstream accepted the stable auxiliary Run but no result committed;
    // C2 the old claim expires; C3 canonical startup restores the Session;
    // C4 the explicit unknown terminal survives another process restart.
    // U1 C1+C2+C3 => ExecutionIndeterminate, same intent/Run identity, no second
    // inference and no fabricated mutations/receipt/Memory version. U2 U1+C4 =>
    // identical terminal after a completed startup scan, still no inference.
    // The old unconditional success oracle confused local process exit with
    // remote termination. Durable success and Extracted/Stored crash recovery
    // are positive controls in the existing durable/stage-recovery E2Es.
    // The SDK history read only observes the disposable wire projection after
    // restart. It does not enqueue a command: recovery remains owned by the same
    // Coordinator lifecycle supervisor as production, never by a protocol-specific
    // trigger or by the history observer itself.
    assert.ok((await reply(session.id)).includes(MARKER), 'committed Session rehydrated');
    const rows = await waitForValue(
      () => extractionRows(database),
      (rows) => rows.length === 1 && rows[0].status === 'execution_indeterminate',
      'U1 restart must classify the original unknown extraction',
      { timeoutMs: 30_000 },
    );
    const terminal = rows[0];
    assert.equal(terminal.intent_id, original.intent_id, 'U1 stable intent');
    assert.equal(terminal.intent.auxiliary_run_id, original.intent.auxiliary_run_id, 'U1 stable auxiliary Run');
    assert.equal(terminal.intent.auxiliary_thread_id, original.intent.auxiliary_thread_id, 'U1 stable auxiliary Thread');
    assert.deepEqual(terminal.intent.mutations, [], 'U1 no invented Memory effects');
    assert.equal(terminal.intent.receipt ?? null, null, 'U1 no invented receipt');
    assert.equal(terminal.intent.claim_owner ?? null, null, 'U1 claim retired');
    assert.equal(upstream.received, received, 'U1 inference is not repeated');

    const versions = await client.get(`/v1/memory_stores/${store.id}/memory_versions`, {
      headers: MEMORY_HEADERS,
    });
    assert.equal(versions.data.length, 0, 'U1 unknown inference commits no Memory version');

    await hardKill(b.server);
    servers.pop();
    const capture = { buf: '' };
    const c = spawnServer('memory', PORT, { ...env, RUST_LOG: 'awaken_session_application=info' }, process.env, capture);
    servers.push(c.server);
    await waitForPort(PORT);
    await waitForValue(() => capture.buf, (text) => text.includes('Session recovery scan completed'),
      'U2 the canonical startup supervisor must actually scan after the second crash');
    assert.deepEqual(extractionRows(database), [terminal], 'U2 terminal identity and revision remain unchanged');
    assert.equal(upstream.received, received, 'U2 terminal restart does not repeat inference');
    pass('unknown extraction survives two restarts without replay or fabricated Memory');
    console.log('E2E PASS: in-flight Memory crash retains the exact unknown outcome.');
  } finally {
    for (const server of servers) await stopServer(server);
    upstream.close();
    cleanupFixtureTree(STORE_DIR);
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
