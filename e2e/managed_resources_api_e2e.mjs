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

      // ── memory-store API: create → read (empty) → 404 on unknown ───────────────
      const mem = await client.post('/v1/memory_stores');
      assert.ok(mem.id?.startsWith('memstore_'), 'memory store gets a memstore_ id');
      const read = await client.get(`/v1/memory_stores/${mem.id}`);
      assert.equal(read.id, mem.id);
      assert.equal(read.content, '', 'a fresh memory store is empty');
      assert.equal(read.size_bytes, 0);
      pass(`memory store created and reads back empty: ${mem.id}`);

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
