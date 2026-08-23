// Memory_store RESOURCE durability across a real process restart (ADR-0038).
//
// The ADR-0038 MemoryStore family gives a session a stable, mutable id it mounts
// read-write: the agent edits the realized file and the MemoryMount guard reconciles
// the copy at Session release under the same id, so a memory written in one session is visible to the
// next. This test proves that write-back is *durable* — it must survive the server
// process dying, not just live in one process's heap.
//
// Flow: create a memory store, run a session whose deterministic model writes a
// marker into the mount, release the Session, assert the `/memories` content
// reflects it, then KILL the server and start a fresh one over the SAME storage
// dir. The marker must still be there. A purely in-memory store loses it on
// restart — that is the completeness gap this test exists to catch.
//
// Deterministic (`memory-resource` mode) so it runs in CI without an API key.
//
// Run: (from e2e/)  node managed_memory_store_durable_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree,
  hasEndTurn,
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38211);
const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const STORE_DIR = `/tmp/awaken-memstore-durable-e2e-${process.pid}`;
const MARKER = 'PERSISTED_MARKER_7788';

let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

async function memContent(id) {
  // Observation rule O1: a successful list projects the durable contents;
  // O2: any HTTP/decode failure must fail the test at its real cause. Returning
  // an empty string for O2 would falsely classify transport failure as an empty
  // MemoryStore and hide the failing boundary.
  // Decision rule O3: content assertion -> request `view=full`; the canonical
  // default `basic` view intentionally returns `content: null` and therefore
  // cannot distinguish an existing Memory from an empty store.
  const page = await client.get(`/v1/memory_stores/${id}/memories?view=full`, {
    headers: MEMORY_HEADERS,
  });
  return (page?.data ?? []).map((memory) => memory.content ?? '').join('\n');
}

async function main() {
  cleanupFixtureTree(STORE_DIR);
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const servers = [];
  const upstream = await startUpstream('memoryResource');
  try {
    // ---- server A: write into a mounted memory store; release reconciles it ----
    const a = spawnServer('memory-resource', PORT, { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR, ...realServerEnv('memoryResource', upstream, { mode: 'memory-resource' }) });
    servers.push(a.server);
    await waitForPort(PORT);

    const mem = await client.post('/v1/memory_stores', {
      body: { name: 'durable-memory-store' },
      headers: MEMORY_HEADERS,
    });
    assert.ok(mem.id, 'POST /v1/memory_stores returned an id');

    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{ type: 'memory_store', memory_store_id: mem.id, mount_path: '/memory' }],
      betas: BETAS,
    });
    // M0 lifecycle: C1=exact write-task receipt; C2=requires_action with exact
    // tool ids; C3=exact allow batch; C4=end_turn. E1=successful write/read
    // results before release. Constraint: approvals are scenario writes and all
    // observation is receipt-scoped. C1+C2=>approve; C1+C2+C3+C4=>E1.
    const taskReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: MARKER }] }],
      betas: BETAS,
    });

    // Drive write -> approval at committed boundaries, then release the mount once.
    let observation = await waitForSessionEventReceipt(
      client,
      session.id,
      taskReceipt.data[0]?.id,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'session.status_idle'),
      'M0 write task reaches its first committed boundary',
    );
    let completedEvents = observation.events;
    for (let boundary = 0; boundary < 10 && !hasEndTurn(completedEvents); boundary += 1) {
      const idle = [...observation.delta]
        .reverse()
        .find((event) => event.type === 'session.status_idle');
      assert.equal(idle?.stop_reason?.type, 'requires_action', 'M0 nonterminal boundary requires approval');
      const pendingIds = idle.stop_reason.event_ids;
      assert.ok(pendingIds.length > 0, 'M0 requires_action names pending Event ids');
      const decisions = pendingIds.map((id) => {
        const toolUse = completedEvents.find(
          (event) => event.id === id && event.type === 'agent.tool_use',
        );
        assert.equal(toolUse?.evaluated_permission, 'ask', `M0 ${id} is gated`);
        return { type: 'user.tool_confirmation', tool_use_id: id, result: 'allow' };
      });
      const approval = await client.beta.sessions.events.send(session.id, {
        events: decisions,
        betas: BETAS,
      });
      const receiptIds = approval.data.map((event) => event.id);
      observation = await waitForSessionEventReceipt(
        client,
        session.id,
        receiptIds.at(-1),
        BETAS,
        ({ events, delta }) => receiptIds.every((id) => events.some(
          (event) => event.id === id && event.processed_at,
        )) && delta.some((event) => event.type === 'session.status_idle'),
        'M0 approval batch reaches its next committed boundary',
      );
      completedEvents = observation.events;
    }
    assert.ok(hasEndTurn(completedEvents), 'the memory-writing turn completed');
    const toolResults = completedEvents.filter((event) => event.type === 'agent.tool_result');
    const writeResult = toolResults[0];
    assert.ok(writeResult, 'the gated write produced a tool result before release');
    assert.notEqual(
      writeResult.is_error,
      true,
      `the Memory mount write succeeded: ${JSON.stringify(writeResult.content)}`,
    );
    assert.ok(
      JSON.stringify(toolResults.at(-1)?.content).includes(MARKER),
      `the follow-up read observes the exact mounted bytes: ${JSON.stringify(toolResults)}`,
    );
    await client.beta.sessions.delete(session.id, { betas: BETAS });
    const harvested = await waitForValue(
      () => memContent(mem.id),
      (content) => content.includes(MARKER),
      'released Memory mount is harvested into the durable store',
      { timeoutMs: 4_000, pollMs: 200 },
    );
    assert.ok(
      harvested.includes(MARKER),
      `server A harvested the write into the memory store: ${JSON.stringify(harvested)}`,
    );
    pass('Session release reconciled the Memory mount into the store (pre-restart)');

    // ---- restart: kill A, start B over the SAME storage dir ----
    await stopServer(a.server);
    servers.pop();
    const b = spawnServer('memory-resource', PORT, { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR, ...realServerEnv('memoryResource', upstream, { mode: 'memory-resource' }) });
    servers.push(b.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

    const after = await memContent(mem.id);
    assert.ok(
      after.includes(MARKER),
      `the memory store's contents survived the restart (got: ${JSON.stringify(after)})`,
    );
    pass('memory_store contents survived a real process restart');
    console.log('E2E PASS: ADR-0038 memory_store resource is durable across restart.');
  } finally {
    for (const s of servers) await stopServer(s);
    upstream.close();
    // Cleanup cause/effect rules: C1 no retained mount -> recursively remove;
    // C2 live or disconnected FUSE projection -> detach deepest-first, then
    // remove; C3 detach failure -> fail the test. FMECA: raw rmSync maps C2 to
    // EISDIR/ENOTCONN and leaks the fixture after the logically asynchronous
    // Session teardown; the canonical harness is the sole mount-aware owner.
    cleanupFixtureTree(STORE_DIR);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
