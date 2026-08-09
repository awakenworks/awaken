// Freeze file + memory_store resources into a session, then exercise realization.
// The default deterministic scenario owns the Workdir tier: it can realize writable
// Memory but cannot OS-enforce a read-only File, so that combination must fail closed
// before inference. Namespace/container happy paths are covered by their substrate
// suites. Deterministic, CI-safe.

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_BETAS = ['agent-memory-2026-07-22'];
const MEMORY_HEADERS = { 'anthropic-beta': MEMORY_BETAS[0] };

async function main() {
  await withRealServer('echo', 38272, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    // Supply: a content-addressed file + a memory store.
    const file = await client.beta.files.upload({
      file: await toFile(Buffer.from('mounted file bytes'), 'doc.txt'),
      betas: BETAS,
    });
    assert.ok(file.id, 'file uploaded');
    const mem = await client.post('/v1/memory_stores', { headers: MEMORY_HEADERS });
    assert.ok(mem.id, 'memory store created');

    // Resource realization decision table:
    // R1 valid File+Memory bindings -> create/freeze succeeds;
    // R2 Workdir + required read-only File -> turn fails before inference;
    // R3 Workdir + writable Memory only -> turn runs;
    // R4 archived frozen Memory -> next turn rejected before inference.
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      resources: [
        { type: 'file', file_id: file.id, mount_path: '/workspace/doc.txt' },
        { type: 'memory_store', memory_store_id: mem.id, mount_path: '/workspace/memory' },
      ],
      betas: BETAS,
    });
    assert.ok(session.id.startsWith('sesn_'), `session with resources: ${session.id}`);
    pass('session froze file + memory_store bindings');

    await assert.rejects(
      () => client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'work with the files' }] }],
        betas: BETAS,
      }),
      /read-only mount .* requested but backend does not enforce read-only/u,
      'Workdir must not pretend to enforce the frozen read-only File binding',
    );
    const deniedEvents = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) deniedEvents.push(ev.type);
    assert.ok(!deniedEvents.includes('agent.message'), 'read-only realization denial never reaches inference');
    pass('Workdir failed the read-only File realization closed');

    const memorySession = await client.beta.sessions.create({
      agent: 'assistant',
      resources: [{ type: 'memory_store', memory_store_id: mem.id, mount_path: '/workspace/memory' }],
      betas: BETAS,
    });
    await client.beta.sessions.events.send(memorySession.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'work with memory' }] }],
      betas: BETAS,
    });
    const events = [];
    for await (const ev of client.beta.sessions.events.list(memorySession.id, { betas: BETAS })) events.push(ev.type);
    assert.ok(events.includes('agent.message'), `the writable Memory turn ran: ${events}`);
    pass('the Session ran with its writable frozen Memory binding');

    // Lifecycle state is deliberately live. Archiving the store must deny the
    // next use even though the immutable frozen binding still exists.
    await client.beta.memoryStores.archive(mem.id, { betas: MEMORY_BETAS });
    const agentMessagesBeforeDeny = events.filter((type) => type === 'agent.message').length;
    await assert.rejects(
      () =>
        client.beta.sessions.events.send(memorySession.id, {
          events: [{ type: 'user.message', content: [{ type: 'text', text: 'try archived memory' }] }],
          betas: BETAS,
        }),
      (error) => error.status === 400 && error.error?.error?.message?.includes('not active'),
    );
    const afterArchive = [];
    for await (const ev of client.beta.sessions.events.list(memorySession.id, { betas: BETAS })) afterArchive.push(ev);
    assert.equal(
      afterArchive.filter((ev) => ev.type === 'agent.message').length,
      agentMessagesBeforeDeny,
      'the denied turn never reaches the model',
    );
    pass('live lifecycle state denied reuse without changing the frozen config');

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
