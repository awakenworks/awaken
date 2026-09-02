// Freeze file + memory_store resources into a session, then exercise realization.
// The default deterministic scenario owns the Workdir tier: it can realize writable
// Memory but cannot OS-enforce a read-only File, so that combination must fail closed
// before inference. Namespace/container happy paths are covered by their substrate
// suites. Deterministic, CI-safe.

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import {
  assertPendingReceiptHasNoRuntimeEffects,
  pass,
  waitForSessionEventReceipt,
  withRealServer,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_BETAS = ['agent-memory-2026-07-22'];
const MEMORY_HEADERS = { 'anthropic-beta': MEMORY_BETAS[0] };
const EXECUTION_OR_TERMINAL_EVENT_TYPES = new Set([
  'agent.message',
  'agent.mcp_tool_use',
  'agent.mcp_tool_result',
  'agent.tool_use',
  'agent.tool_result',
  'session.error',
  'session.status_running',
  'session.thread_status_running',
  'session.status_idle',
  'session.thread_status_idle',
  'session.status_terminated',
  'session.thread_status_terminated',
  'session.usage',
  'span.model_request_start',
  'span.model_request_end',
]);

async function main() {
  await withRealServer('echo', 38272, async (base, upstream) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    // Supply: a content-addressed file + a memory store.
    const file = await client.beta.files.upload({
      file: await toFile(Buffer.from('mounted file bytes'), 'doc.txt'),
      betas: BETAS,
    });
    assert.ok(file.id, 'file uploaded');
    const mem = await client.post('/v1/memory_stores', {
      body: { name: 'resource-mount-memory' },
      headers: MEMORY_HEADERS,
    });
    assert.ok(mem.id, 'memory store created');

    // Resource realization decision table:
    // R1 valid File+Memory bindings -> create/freeze succeeds;
    // R2 Workdir + required read-only File -> turn fails before inference;
    // R3 Workdir + writable Memory only -> turn runs;
    // R4 archived frozen Memory -> next turn retained without inference.
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [
        { type: 'file', file_id: file.id, mount_path: '/workspace/doc.txt' },
        { type: 'memory_store', memory_store_id: mem.id },
      ],
      betas: BETAS,
    });
    assert.ok(session.id.startsWith('sesn_'), `session with resources: ${session.id}`);
    pass('session froze file + memory_store bindings');

    const modelRequestsBeforeDeny = upstream.requests.length;
    assert.deepEqual(
      (await client.get(`/v1/memory_stores/${mem.id}/memories`, { headers: MEMORY_HEADERS })).data,
      [],
      'R2 starts with an empty MemoryStore',
    );
    // Read-only realization causes: C1=the User batch is durably admitted;
    // C2=Workdir cannot enforce the frozen read-only File mount; C3=one bounded
    // reconciliation window elapses. Effects: E1=admission returns the exact
    // unprocessed receipt; E2=events.list exposes it exactly once as pending;
    // E3=the Session remains idle/nonterminal; E4=no model/tool/terminal
    // effect or MemoryStore mutation occurs. K: the Session root owns retryable
    // command provenance while events.list owns committed history. Decision
    // R2a C1+C2=>E1; R2b C1+C2+C3=>E2+E3+E4.
    const deniedReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'work with the files' }] }],
      betas: BETAS,
    });
    const acceptedDenied = deniedReceipt.data[0];
    assert.equal(acceptedDenied?.type, 'user.message', 'R2a exact User Event receipt family');
    assert.equal(acceptedDenied?.processed_at, null, 'R2a capability failure is not falsely processed');
    await new Promise((resolve) => setTimeout(resolve, 750));
    const deniedEvents = [];
    for await (const event of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
      deniedEvents.push(event);
    }
    assertPendingReceiptHasNoRuntimeEffects({
      history: deniedEvents,
      receiptId: acceptedDenied.id,
      forbiddenEventTypes: EXECUTION_OR_TERMINAL_EVENT_TYPES,
      description: 'R2b denied read-only activation',
    });
    assert.equal(
      (await client.beta.sessions.retrieve(session.id, { betas: BETAS })).status,
      'idle',
      'R2b capability failure remains idle and nonterminal',
    );
    assert.equal(upstream.requests.length, modelRequestsBeforeDeny, 'R2b no Provider request');
    assert.deepEqual(
      (await client.get(`/v1/memory_stores/${mem.id}/memories`, { headers: MEMORY_HEADERS })).data,
      [],
      'R2b denied activation does not mutate the bound MemoryStore',
    );
    pass('Workdir retained the failed read-only realization without execution effects');

    const memorySession = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{ type: 'memory_store', memory_store_id: mem.id }],
      betas: BETAS,
    });
    // C1=exact writable-Memory receipt; C2=reply+terminal. E1=C2 after C1.
    // K: negative mount/lifecycle effects retain their durable User receipts;
    // only this success path waits for reply+terminal. Decision M1 C1&&!C2=>
    // retry; M2 C1+C2=>assert execution.
    const receipt = await client.beta.sessions.events.send(memorySession.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'work with memory' }] }],
      betas: BETAS,
    });
    const receiptId = receipt.data[0]?.id;
    assert.equal(typeof receiptId, 'string', 'M1 exact writable-Memory User Event receipt');
    const { events: listed } = await waitForSessionEventReceipt(
      client,
      memorySession.id,
      receiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.message')
        && delta.some((event) => event.type === 'session.status_idle'),
      'M1 writable-Memory Run to commit',
    );
    const events = listed.map((event) => event.type);
    assert.ok(events.includes('agent.message'), `the writable Memory turn ran: ${events}`);
    pass('the Session ran with its writable frozen Memory binding');

    // Lifecycle state is deliberately live. Archiving the store must deny the
    // next use even though the immutable frozen binding still exists.
    const archivedMemory = await client.beta.memoryStores.archive(mem.id, { betas: MEMORY_BETAS });
    assert.ok(archivedMemory.archived_at, 'R4 archived MemoryStore lifecycle state');
    const modelRequestsBeforeArchiveDeny = upstream.requests.length;
    const resourcesBeforeArchiveDeny = [];
    for await (const resource of client.beta.sessions.resources.list(memorySession.id, { betas: BETAS })) {
      resourcesBeforeArchiveDeny.push(resource);
    }
    const eventsBeforeArchiveDeny = listed;
    // Archived-resource causes: C1=the next User batch is durably admitted;
    // C2=the frozen binding resolves to an archived MemoryStore; C3=one bounded
    // reconciliation window elapses. Effects: E1=return the exact unprocessed
    // admission receipt; E2=exclude the unanchored command from committed
    // history; E3=keep the Session idle/nonterminal and publish no new execution
    // or terminal effect; E4=leave the frozen Resource binding unchanged.
    // K: live Resource lifecycle is checked during realization, after admission;
    // archival does not revoke or delete the already accepted command. Decision
    // R4a C1+C2=>E1; R4b C1+C2+C3=>E2+E3+E4.
    const archiveDeniedReceipt = await client.beta.sessions.events.send(memorySession.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'try archived memory' }] }],
      betas: BETAS,
    });
    const acceptedArchiveDenied = archiveDeniedReceipt.data[0];
    assert.equal(acceptedArchiveDenied?.type, 'user.message', 'R4a exact User Event receipt family');
    assert.equal(
      acceptedArchiveDenied?.processed_at,
      null,
      'R4a archived-resource failure is not falsely processed',
    );
    await new Promise((resolve) => setTimeout(resolve, 750));
    const afterArchive = [];
    for await (const event of client.beta.sessions.events.list(memorySession.id, { betas: BETAS })) {
      afterArchive.push(event);
    }
    assertPendingReceiptHasNoRuntimeEffects({
      history: afterArchive,
      priorHistory: eventsBeforeArchiveDeny,
      receiptId: acceptedArchiveDenied.id,
      forbiddenEventTypes: EXECUTION_OR_TERMINAL_EVENT_TYPES,
      description: 'R4b archived Resource reuse',
    });
    assert.equal(
      (await client.beta.sessions.retrieve(memorySession.id, { betas: BETAS })).status,
      'idle',
      'R4b archived-resource failure remains idle and nonterminal',
    );
    assert.equal(upstream.requests.length, modelRequestsBeforeArchiveDeny, 'R4b no Provider request');
    const resourcesAfterArchiveDeny = [];
    for await (const resource of client.beta.sessions.resources.list(memorySession.id, { betas: BETAS })) {
      resourcesAfterArchiveDeny.push(resource);
    }
    assert.deepEqual(
      resourcesAfterArchiveDeny,
      resourcesBeforeArchiveDeny,
      'R4b denied reuse does not change the frozen Resource binding',
    );
    pass('live lifecycle state retained denied reuse without execution or Resource effects');

    // A missing file reference fails the mount closed.
    const bad = await fetch(`${base}/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
      body: JSON.stringify({
        agent: 'assistant',
        resources: [{ type: 'file', file_id: 'file_does_not_exist', mount_path: '/x' }],
      }),
    });
    assert.ok(bad.status >= 400, `a missing file resource fails the create (got ${bad.status})`);
    pass('mounting a missing file resource fails closed');
  });
  console.log('E2E PASS: file + memory_store resource mounting into a session.');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
