// Session-scoped resource lifecycle (Managed Agents contract, ADR-0038), driven
// through the official Anthropic TS SDK's `sessions.resources` sub-API. This
// covers the corrected create-time-vs-live distinction that the mount-at-creation
// test (managed_resource_mount_e2e.mjs) does not:
//
//   • only file can be attached to a LIVE session
//   • github_repository / memory_store bind at creation only
//
// Deterministic (echo model, no API key), so it runs in the keyless coverage arm.
//
// Cause graph:
// official create-time union without output-only Memory mount_path -> catalog-
// derived exact Session manifest -> sandbox realization;
// typed live File add/delete -> prepare/apply/commit -> durable projection;
// non-File add or raw-token update -> admission reject -> no Runtime/state effect.
//
// Decision table:
// | Rule | Operation | Shape | Expected behavior | Observable effect |
// | R1 | create | Memory identity/Repository | accept | listed with frozen server-derived Memory path |
// | R1X | create | Memory with client mount_path | 400 | no Session root (ResourceInput contract owner) |
// | R2 | live add | File | accept | exact mount appears |
// | R3 | live add | Memory/Repository | 400 | manifest unchanged |
// | R4 | update | authorization_token/unknown | 400 | manifest unchanged |
// | R5 | delete | addressable File/Repository | accept | resource disappears |

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { FILES_BETA, withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01', FILES_BETA];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };

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
    const mem = await client.post('/v1/memory_stores', {
      body: { name: 'resource-lifecycle-memory' },
      headers: MEMORY_HEADERS,
    });

    // ── create-time backfill: a session created WITH resources echoes them ──────
    const seeded = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [
        {
          type: 'memory_store',
          memory_store_id: mem.id,
          instructions: 'notes',
        },
      ],
      betas: BETAS,
    });
    assert.equal(seeded.resources?.length, 1, 'create-time memory_store is backfilled');
    assert.equal(seeded.resources[0].type, 'memory_store');
    assert.equal(seeded.resources[0].memory_store_id, mem.id);
    assert.equal(
      seeded.resources[0].mount_path,
      '/mnt/memory/resource-lifecycle-memory',
      'the catalog-owned MemoryStore name derives the frozen output path',
    );
    assert.equal(seeded.resources[0].id, undefined, 'official memory resources have no synthetic id');
    assert.equal((await listResources(client, seeded.id)).length, 1, 'create-time resource is listed');
    pass('create-time resources are backfilled on the session and listed');

    await assert.rejects(
      () => client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        resources: [{
          type: 'memory_store',
          memory_store_id: mem.id,
          mount_path: '/client-owned-path',
        }],
        betas: BETAS,
      }),
      (error) => error?.status === 400 && /mount_path|unknown field/u.test(error.message),
      'R1X client-authored Memory mount_path fails the closed ResourceInput union',
    );
    pass('client-authored MemoryStore mount_path is rejected at typed admission');

    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{
        type: 'github_repository',
        url: 'https://github.com/owner/repo',
        mount_path: '/workspace/repository',
      }],
      betas: BETAS,
    });
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

    const repoRes = session.resources.find((resource) => resource.type === 'github_repository');
    assert.ok(repoRes?.id);
    assert.equal(repoRes.type, 'github_repository');
    pass('github_repository is frozen at Session creation');

    const retrievedRepo = await client.beta.sessions.resources.retrieve(repoRes.id, {
      session_id: session.id,
      betas: BETAS,
    });
    assert.equal(retrievedRepo.id, repoRes.id);
    const retrievedFile = await client.beta.sessions.resources.retrieve(fileRes.id, {
      session_id: session.id,
      betas: BETAS,
    });
    assert.equal(retrievedFile.type, 'file');
    assert.equal(retrievedFile.file_id, file.id);
    await assert.rejects(
      () => client.beta.sessions.resources.update(fileRes.id, {
        session_id: session.id,
        authorization_token: 'file-cannot-use-token', // awaken-allow: secret
        betas: BETAS,
      }),
      (e) => e.status === 400,
      'repository authorization cannot be applied to a File binding',
    );
    await assert.rejects(
      () => client.beta.sessions.resources.update(repoRes.id, {
        session_id: session.id,
        authorization_token: 'must-not-enter', // awaken-allow: secret
        betas: BETAS,
      }),
      (e) => e.status === 400,
      'raw repository credentials fail typed admission',
    );
    assert.equal((await listResources(client, session.id)).find((r) => r.id === repoRes.id).mount_path,
      '/workspace/repository', 'R4 rejected update preserves the manifest');

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

    await client.beta.sessions.resources.delete(repoRes.id, {
      session_id: session.id,
      betas: BETAS,
    });
    assert.deepEqual(await listResources(client, session.id), []);
    await assert.rejects(
      () => client.beta.sessions.resources.retrieve(repoRes.id, {
        session_id: session.id,
        betas: BETAS,
      }),
      (e) => e.status === 404,
      'retired Repository binding is no longer visible',
    );
    pass('repository detach retires its resource-catalog aggregate');
  });

  console.log('E2E PASS: typed Session resources (File live; Repository/Memory create-time only).');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
