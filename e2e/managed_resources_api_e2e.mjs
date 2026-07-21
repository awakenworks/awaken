// Deterministic (no API key) resource-plane HTTP e2e (ADR-0038): exercises the
// Files API and memory-store API surfaces of awaken-server directly through
// the official Anthropic TypeScript SDK, with no model in the loop. This complements
// the live-model `managed_resources_e2e.mjs` (which proves mount/read/write/harvest
// with a real model) by covering the pure request/response paths — metadata, raw
// download, empty + unknown scopes, memory create/read, and the 404s — that a model
// run does not deterministically reach.
//
// Runs in the default (keyless) coverage arm, so it always contributes to the
// resource-surface coverage figure.

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];

async function request(baseUrl, method, route, body) {
  const response = await fetch(`${baseUrl}${route}`, {
    method,
    headers: body === undefined ? undefined : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let value = text;
  try { value = JSON.parse(text); } catch {}
  return { status: response.status, body: value };
}

async function main() {
  try {
    await withRealServer('echo', 38138, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // ── Files API: upload → metadata → download → idempotency ──────────────────
      const body = 'resource-plane-fixture-bytes';
      const up = await client.beta.files.upload({
        file: await toFile(Buffer.from(body), 'fixture.txt'),
        betas: BETAS,
      });
      assert.ok(up.id, 'upload returns a content id');
      assert.equal(up.size_bytes, body.length, 'upload reports byte size');
      pass(`file uploaded: ${up.id.slice(0, 12)} (${up.size_bytes} bytes)`);

      // GET /v1/files/:id — metadata (presence + size).
      const meta = await client.get(`/v1/files/${up.id}`);
      assert.equal(meta.id, up.id);
      assert.equal(meta.size_bytes, body.length);
      pass('file metadata reflects stored size');

      // GET /v1/files/:id/content — raw bytes (what files.download reads).
      const dl = await client.beta.files.download(up.id, { betas: BETAS });
      assert.equal(await dl.text(), body, 'download returns the exact bytes');
      pass('file content downloaded verbatim');

      // Content-addressed store: re-uploading identical bytes yields the same id.
      const up2 = await client.beta.files.upload({
        file: await toFile(Buffer.from(body), 'again.txt'),
        betas: BETAS,
      });
      assert.equal(up2.id, up.id, 'equal bytes ⇒ identical content id (idempotent)');
      pass('re-upload is idempotent (same content id)');

      // A missing id is a 404.
      await assert.rejects(
        () => client.get('/v1/files/blob_does_not_exist'),
        (e) => String(e).includes('404'),
        'unknown file id is a 404',
      );
      pass('unknown file id → 404');

      // ── files.list scoping: empty without a scope, empty for an unknown scope ───
      const noScope = [];
      for await (const f of client.beta.files.list({ betas: BETAS })) noScope.push(f);
      assert.equal(noScope.length, 0, 'no scope_id ⇒ empty list (files are session-scoped)');
      const unknownScope = [];
      for await (const f of client.beta.files.list({ scope_id: 'sesn_absent', betas: BETAS })) {
        unknownScope.push(f);
      }
      assert.equal(unknownScope.length, 0, 'unknown scope ⇒ empty list');
      pass('files.list is empty without a scope and for an unknown scope');

      const deletedFile = await request(baseUrl, 'DELETE', `/v1/files/${up.id}`);
      assert.equal(deletedFile.status, 200);
      assert.equal(deletedFile.body.type, 'file_deleted');
      await assert.rejects(
        () => client.get(`/v1/files/${up.id}`),
        (e) => String(e).includes('404'),
        'logical File deletion denies reads before asynchronous reclamation',
      );
      assert.equal((await request(baseUrl, 'DELETE', `/v1/files/${up.id}`)).status, 404);
      pass('file deletion commits immediate logical denial and is idempotently absent');

      // ── MemoryStore: catalog patch + CAS heads + versions + redaction + delete ─
      const mem = await client.post('/v1/memory_stores', {
        body: {
          name: 'embedded-memory',
          description: 'initial',
          metadata: { phase: 'created', remove_me: 'yes' },
        },
      });
      assert.ok(mem.id?.startsWith('memstore_'), 'memory store gets a memstore_ id');
      const read = await client.get(`/v1/memory_stores/${mem.id}`);
      assert.equal(read.id, mem.id);
      assert.equal(read.content, undefined, 'store definition does not duplicate mutable content');
      assert.equal(read.size_bytes, undefined);
      const memories = await client.get(`/v1/memory_stores/${mem.id}/memories`);
      assert.deepEqual(memories.data, [], 'a fresh memory store has no memory heads');
      pass(`memory store created and reads back empty: ${mem.id}`);

      const stores = await request(baseUrl, 'GET', '/v1/memory_stores');
      assert.equal(stores.status, 200);
      assert.ok(stores.body.data.some((store) => store.id === mem.id));
      const patched = await request(baseUrl, 'POST', `/v1/memory_stores/${mem.id}`, {
        description: 'updated',
        metadata: { phase: 'updated', remove_me: null },
      });
      assert.equal(patched.status, 200);
      assert.equal(patched.body.description, 'updated');
      assert.deepEqual(patched.body.metadata, { phase: 'updated' });

      assert.equal(
        (await request(baseUrl, 'POST', `/v1/memory_stores/${mem.id}/memories`, {
          content: 'missing path',
        })).status,
        400,
      );
      const created = await request(baseUrl, 'POST', `/v1/memory_stores/${mem.id}/memories`, {
        path: '/fact.md',
        content: 'embedded v1',
      });
      assert.equal(created.status, 200);
      const memoryId = created.body.id;
      const initialSha = created.body.content_sha256;
      assert.equal(
        (await request(baseUrl, 'POST', `/v1/memory_stores/${mem.id}/memories`, {
          path: '/fact.md',
          content: 'duplicate path',
        })).status,
        409,
      );
      assert.equal(
        (await request(baseUrl, 'POST', `/v1/memory_stores/${mem.id}/memories/${memoryId}`, {
          content: 'stale must not win',
          precondition: { content_sha256: 'stale' },
        })).status,
        409,
      );
      const updated = await request(
        baseUrl,
        'POST',
        `/v1/memory_stores/${mem.id}/memories/${memoryId}`,
        {
          path: '/renamed.md',
          content: 'embedded v2',
          precondition: { content_sha256: initialSha },
        },
      );
      assert.equal(updated.status, 200);
      assert.equal(updated.body.path, '/renamed.md');
      const basic = await request(
        baseUrl,
        'GET',
        `/v1/memory_stores/${mem.id}/memories?path_prefix=/&view=basic`,
      );
      assert.equal(basic.status, 200);
      assert.equal(basic.body.data[0].content, null);
      assert.equal(
        (await request(baseUrl, 'GET', `/v1/memory_stores/${mem.id}/memories/${memoryId}`)).status,
        200,
      );

      const versions = await request(
        baseUrl,
        'GET',
        `/v1/memory_stores/${mem.id}/memory_versions`,
      );
      assert.equal(versions.status, 200);
      assert.ok(versions.body.data.length >= 2);
      const firstVersion = versions.body.data[0].id;
      assert.equal(
        (await request(
          baseUrl,
          'GET',
          `/v1/memory_stores/${mem.id}/memory_versions/${firstVersion}`,
        )).status,
        200,
      );
      const redacted = await request(
        baseUrl,
        'POST',
        `/v1/memory_stores/${mem.id}/memory_versions/${firstVersion}/redact`,
      );
      assert.equal(redacted.status, 200);
      assert.equal(redacted.body.content, null);
      assert.notEqual(redacted.body.redacted_at, null);

      assert.equal(
        (await request(
          baseUrl,
          'DELETE',
          `/v1/memory_stores/${mem.id}/memories/${memoryId}`,
        )).status,
        200,
      );
      assert.equal(
        (await request(baseUrl, 'GET', `/v1/memory_stores/${mem.id}/memories/${memoryId}`)).status,
        404,
      );

      const archived = await request(baseUrl, 'POST', `/v1/memory_stores/${mem.id}/archive`);
      assert.equal(archived.status, 200);
      assert.notEqual(archived.body.archived_at, null);
      assert.equal(
        (await request(baseUrl, 'POST', `/v1/memory_stores/${mem.id}/memories`, {
          path: '/denied.md',
          content: 'must not write',
        })).status,
        404,
      );
      assert.equal((await request(baseUrl, 'DELETE', `/v1/memory_stores/${mem.id}`)).status, 200);
      assert.equal(
        (await request(baseUrl, 'GET', `/v1/memory_stores/${mem.id}/memories`)).status,
        404,
      );
      pass('embedded MemoryStore preserves CAS, version, redaction, and lifecycle invariants');

      await assert.rejects(
        () => client.get('/v1/memory_stores/memstore_absent'),
        (e) => String(e).includes('404'),
        'unknown memory store is a 404',
      );
      pass('unknown memory store → 404');
    });

    console.log('E2E PASS: resource-plane HTTP surface (files + memory-store) verified deterministically.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
