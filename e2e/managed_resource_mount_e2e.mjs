// Mount file + memory_store resources into a session: the resources realize into
// the sandbox at session-prepare time (independent of the model), so this drives
// the managed state layer's resource parsing (file / memory_store branches) + the
// host's resource realization — the path the key-gated managed_resources e2e
// skips. A turn then runs over the mounted session. Deterministic, CI-safe.

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  await withRealServer('echo', 38272, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    // Supply: a content-addressed file + a memory store.
    const file = await client.beta.files.upload({
      file: await toFile(Buffer.from('mounted file bytes'), 'doc.txt'),
      betas: BETAS,
    });
    assert.ok(file.id, 'file uploaded');
    const mem = await client.post('/v1/memory_stores');
    assert.ok(mem.id, 'memory store created');

    // A session mounting both resources — realized into the sandbox at prepare.
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      resources: [
        { type: 'file', file_id: file.id, mount_path: '/workspace/doc.txt' },
        { type: 'memory_store', memory_store_id: mem.id, mount_path: '/workspace/memory' },
      ],
      betas: BETAS,
    });
    assert.ok(session.id.startsWith('sesn_'), `session with resources: ${session.id}`);
    pass('session created with file + memory_store resources mounted');

    // A turn runs over the mounted session (the sandbox is realized).
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'work with the files' }] }],
      betas: BETAS,
    });
    const events = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev.type);
    assert.ok(events.includes('agent.message'), `the turn ran with resources mounted: ${events}`);
    pass('a turn ran over a session with mounted file + memory_store resources');

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
