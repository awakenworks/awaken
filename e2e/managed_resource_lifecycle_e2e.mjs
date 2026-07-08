// Session-scoped resource lifecycle (Managed Agents contract, ADR-0038), driven
// through the official Anthropic TS SDK's `sessions.resources` sub-API. This
// covers the corrected create-time-vs-live distinction that the mount-at-creation
// test (managed_resource_mount_e2e.mjs) does not:
//
//   • file / github_repository CAN be attached (and file detached) on a LIVE session
//   • memory_store CANNOT be attached to a running session — it binds at creation
//     only, so `resources.add({type:'memory_store'})` fails closed with a 400.
//
// Deterministic (echo model, no API key), so it runs in the keyless coverage arm.
//
// NOTE: detaching a memory_store is likewise rejected in the runtime, but that
// guard is not reachable over the wire yet — a creation-time memory_store is not
// reflected into `Session.resources`, and adding one is blocked here, so no
// memory resource id ever exists to DELETE. That path gets a wire test once the
// session-resource backfill lands; the runtime guard is covered by the Rust unit
// test `session_resources::memory_store_cannot_attach_to_a_running_session`.

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];

async function listResources(client, sessionId) {
  const out = [];
  for await (const r of client.beta.sessions.resources.list(sessionId, { betas: BETAS })) out.push(r);
  return out;
}

async function main() {
  await withRealServer('echo', 38291, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    const file = await client.beta.files.upload({
      file: await toFile(Buffer.from('live-attach bytes'), 'doc.txt'),
      betas: BETAS,
    });
    const mem = await client.post('/v1/memory_stores');

    // ── create-time backfill: a session created WITH resources echoes them ──────
    const seeded = await client.beta.sessions.create({
      agent: 'assistant',
      resources: [
        {
          type: 'memory_store',
          memory_store_id: mem.id,
          mount_path: '/mnt/memory/notes',
          instructions: 'notes',
        },
      ],
      betas: BETAS,
    });
    assert.equal(seeded.resources?.length, 1, 'create-time memory_store is backfilled');
    assert.equal(seeded.resources[0].type, 'memory_store');
    assert.equal(seeded.resources[0].memory_store_id, mem.id);
    assert.ok(
      seeded.resources[0].id && seeded.resources[0].created_at,
      'the backfilled entry is SDK-decodable (id + created_at)',
    );
    assert.equal((await listResources(client, seeded.id)).length, 1, 'create-time resource is listed');
    pass('create-time resources are backfilled on the session and listed');

    const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
    assert.ok(session.id.startsWith('sesn_'), `session created: ${session.id}`);

    // ── file: attach to a live session ─────────────────────────────────────────
    const fileRes = await client.beta.sessions.resources.add(session.id, {
      type: 'file',
      file_id: file.id,
      mount_path: '/workspace/data.csv',
      betas: BETAS,
    });
    assert.equal(fileRes.type, 'file');
    assert.ok(fileRes.id, 'the attached file mount gets an id');
    assert.ok(fileRes.created_at && fileRes.updated_at, 'the attached entry carries timestamps');
    pass(`file attached to a live session: ${fileRes.id}`);

    // ── github_repository: attach to a live session ────────────────────────────
    const repoRes = await client.beta.sessions.resources.add(session.id, {
      type: 'github_repository',
      url: 'https://github.com/owner/repo',
      authorization_token: 'ghp_e2e', // awaken-allow: secret
      betas: BETAS,
    });
    assert.equal(repoRes.type, 'github_repository');
    pass('github_repository attached to a live session');

    assert.equal((await listResources(client, session.id)).length, 2, 'both live mounts are listed');

    // ── memory_store: rejected on a running session (create-time only) ─────────
    await assert.rejects(
      () =>
        client.beta.sessions.resources.add(session.id, {
          type: 'memory_store',
          memory_store_id: mem.id,
          betas: BETAS,
        }),
      (e) => e.status === 400,
      'attaching a memory_store to a running session is a 400',
    );
    pass('memory_store cannot be attached to a running session → 400');
    // The rejected add did not mutate the session's resource set.
    assert.equal((await listResources(client, session.id)).length, 2, 'reject left the mounts unchanged');

    // ── file: detach from a live session ───────────────────────────────────────
    await client.beta.sessions.resources.delete(fileRes.id, { session_id: session.id, betas: BETAS });
    const after = await listResources(client, session.id);
    assert.equal(after.length, 1, 'the file was detached; the repo remains');
    assert.equal(after[0].type, 'github_repository');
    pass('a file resource was detached from a live session');
  });

  console.log('E2E PASS: session resource lifecycle (file/repo live-attachable, memory create-time only).');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
