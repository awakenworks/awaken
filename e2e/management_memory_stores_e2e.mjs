// The memory-stores family, driven by the official Anthropic TypeScript SDK
// (`client.beta.memoryStores.*`, `.memories.*`, `.memoryVersions.*`): the store
// CRUD + archive, the memories subresource (create/retrieve/update/list/delete
// with a content_sha256 precondition), and memory_versions (retrieve/list/redact).
// Any wire-shape drift from the official `BetaManagedAgentsMemoryStore` /
// `BetaManagedAgentsMemory` / `BetaManagedAgentsMemoryVersion` types surfaces as an
// SDK decode error.
//
// Run: (from e2e/)  node management_memory_stores_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function json(baseUrl, method, route, body) {
  const response = await fetch(`${baseUrl}${route}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function main() {
  try {
    await withScenarioServer('management', 'mcp', 38144, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // -- Store CRUD --------------------------------------------------------
      const store = await client.beta.memoryStores.create({
        name: 'notes',
        description: 'my notes',
        metadata: { a: '1' },
        betas: BETAS,
      });
      assert.equal(store.type, 'memory_store');
      assert.equal(store.name, 'notes');
      pass('beta.memoryStores.create -> BetaManagedAgentsMemoryStore');

      const gotStore = await client.beta.memoryStores.retrieve(store.id, { betas: BETAS });
      assert.equal(gotStore.id, store.id);

      const upStore = await client.beta.memoryStores.update(store.id, {
        description: 'updated notes',
        metadata: { a: null, b: '2' },
        betas: BETAS,
      });
      assert.equal(upStore.description, 'updated notes');
      assert.equal(upStore.metadata.b, '2');
      assert.ok(!('a' in upStore.metadata), 'null metadata patch removes the key');
      pass('beta.memoryStores.retrieve / update');

      const storeIds = (await drain(client.beta.memoryStores.list({ betas: BETAS }))).map((s) => s.id);
      assert.ok(storeIds.includes(store.id));
      pass('beta.memoryStores.list -> PageCursor<BetaManagedAgentsMemoryStore>');

      // -- Versioned resource behavior -------------------------------------
      const configRoute = `/v1/memory_stores/${store.id}/config`;
      const initialConfig = await json(baseUrl, 'GET', configRoute);
      assert.equal(initialConfig.status, 200);
      assert.equal(initialConfig.body.version, 1);
      assert.deepEqual(initialConfig.body.recall_policy, { enabled: true, max_results: 10 });

      assert.equal((await json(baseUrl, 'POST', configRoute, { recall_policy: { enabled: false } })).status, 400);
      assert.equal((await json(baseUrl, 'POST', configRoute, { expected_config_version: 1 })).status, 400);
      assert.equal(
        (
          await json(baseUrl, 'POST', configRoute, {
            expected_config_version: 1,
            recall_policy: { enabled: 'not-a-boolean' },
          })
        ).status,
        400,
      );
      const publishedConfig = await json(baseUrl, 'POST', configRoute, {
        expected_config_version: 1,
        recall_policy: { enabled: false, max_results: 3 },
        extraction_policy: { enabled: false },
        retention_policy: { retention_days: 7 },
      });
      assert.equal(publishedConfig.status, 200);
      assert.equal(publishedConfig.body.version, 2);
      assert.deepEqual(publishedConfig.body.recall_policy, { enabled: false, max_results: 3 });
      assert.deepEqual(publishedConfig.body.extraction_policy, { enabled: false });
      assert.deepEqual(publishedConfig.body.retention_policy, { retention_days: 7 });
      assert.equal(
        (
          await json(baseUrl, 'POST', configRoute, {
            expected_config_version: 1,
            extraction_policy: { enabled: true },
          })
        ).status,
        409,
      );
      assert.equal((await json(baseUrl, 'GET', configRoute)).body.version, 2);
      assert.equal(
        (await json(baseUrl, 'GET', `/v1/memory_stores/${store.id}/config_versions/1`)).body.version,
        1,
      );
      assert.equal(
        (await json(baseUrl, 'GET', `/v1/memory_stores/${store.id}/config_versions/not-an-int`)).status,
        400,
      );
      assert.equal(
        (await json(baseUrl, 'GET', `/v1/memory_stores/${store.id}/config_versions/404`)).status,
        404,
      );
      assert.equal((await json(baseUrl, 'GET', '/v1/memory_stores/missing/config')).status, 404);
      pass('MemoryStore behavior publishes once with CAS and retains immutable config v1');

      // -- Memories ----------------------------------------------------------
      const mem = await client.beta.memoryStores.memories.create(store.id, {
        path: '/notes.md',
        content: 'first',
        betas: BETAS,
      });
      assert.equal(mem.type, 'memory');
      assert.equal(mem.path, '/notes.md');
      assert.ok(mem.content_sha256.length === 64, 'content_sha256 is a 64-char hex digest');
      assert.equal(mem.content_size_bytes, 5);
      pass('beta.memoryStores.memories.create -> BetaManagedAgentsMemory');

      const gotMem = await client.beta.memoryStores.memories.retrieve(mem.id, {
        memory_store_id: store.id,
        betas: BETAS,
      });
      assert.equal(gotMem.id, mem.id);

      // Update with a stale precondition -> 409, then with the correct one.
      await assert.rejects(
        () =>
          client.beta.memoryStores.memories.update(mem.id, {
            memory_store_id: store.id,
            content: 'nope',
            precondition: { type: 'content_sha256', content_sha256: 'deadbeef'.repeat(8) },
            betas: BETAS,
          }),
        (err) => err.status === 409,
      );
      const upMem = await client.beta.memoryStores.memories.update(mem.id, {
        memory_store_id: store.id,
        content: 'second',
        precondition: { type: 'content_sha256', content_sha256: mem.content_sha256 },
        betas: BETAS,
      });
      assert.equal(upMem.content, 'second');
      assert.notEqual(upMem.memory_version_id, mem.memory_version_id, 'update mints a new version');
      pass('beta.memoryStores.memories.update -> precondition (409 stale, ok fresh)');

      const memIds = (await drain(client.beta.memoryStores.memories.list(store.id, { betas: BETAS }))).map(
        (m) => m.id,
      );
      assert.ok(memIds.includes(mem.id));
      pass('beta.memoryStores.memories.list');

      // A second memory in a subtree so path_prefix has something to filter.
      const subMem = await client.beta.memoryStores.memories.create(store.id, {
        path: '/archive/old.md',
        content: 'archived',
        betas: BETAS,
      });

      // path_prefix drills into the subtree, excluding /notes.md.
      const drilled = await drain(
        client.beta.memoryStores.memories.list(store.id, { path_prefix: '/archive', betas: BETAS }),
      );
      assert.deepEqual(
        drilled.map((m) => m.id),
        [subMem.id],
        'path_prefix returns only memories under the prefix',
      );

      // view=basic elides content (content is populated only under the default full view).
      const basic = await drain(
        client.beta.memoryStores.memories.list(store.id, { view: 'basic', betas: BETAS }),
      );
      assert.ok(basic.length >= 2, 'view=basic still lists every memory');
      assert.ok(
        basic.every((m) => m.content === null || m.content === undefined),
        'view=basic omits content',
      );
      pass('beta.memoryStores.memories.list -> path_prefix + view=basic');

      // -- Versions ----------------------------------------------------------
      const versions = await drain(client.beta.memoryStores.memoryVersions.list(store.id, { betas: BETAS }));
      const ops = versions.map((v) => v.operation);
      assert.ok(ops.includes('created') && ops.includes('modified'), `ops: ${ops}`);
      pass(`beta.memoryStores.memoryVersions.list -> ${versions.length} versions`);

      const firstVer = versions[0];
      const gotVer = await client.beta.memoryStores.memoryVersions.retrieve(firstVer.id, {
        memory_store_id: store.id,
        betas: BETAS,
      });
      assert.equal(gotVer.id, firstVer.id);

      const redacted = await client.beta.memoryStores.memoryVersions.redact(firstVer.id, {
        memory_store_id: store.id,
        betas: BETAS,
      });
      assert.ok(redacted.redacted_at, 'redact stamps redacted_at');
      pass('beta.memoryStores.memoryVersions.retrieve / redact');

      // -- Delete memory + archive + delete store ----------------------------
      const delMem = await client.beta.memoryStores.memories.delete(mem.id, {
        memory_store_id: store.id,
        betas: BETAS,
      });
      assert.equal(delMem.type, 'memory_deleted');

      const archived = await client.beta.memoryStores.archive(store.id, { betas: BETAS });
      assert.ok(archived.archived_at);

      const delStore = await client.beta.memoryStores.delete(store.id, { betas: BETAS });
      assert.equal(delStore.type, 'memory_store_deleted');
      assert.equal(
        (
          await json(baseUrl, 'POST', configRoute, {
            expected_config_version: 2,
            extraction_policy: { enabled: true },
          })
        ).status,
        409,
        'a deleted resource cannot publish another configuration',
      );
      pass('beta.memoryStores.memories.delete / archive / delete');
    });

    console.log('E2E PASS: the memory-stores family round-trips through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
