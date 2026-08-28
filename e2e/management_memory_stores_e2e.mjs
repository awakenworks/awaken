// The memory-stores family, driven by the official Anthropic TypeScript SDK
// (`client.beta.memoryStores.*`, `.memories.*`, `.memoryVersions.*`): the store
// CRUD + archive, the memories subresource (create/retrieve/update/list/delete
// with a content_sha256 precondition), and memory_versions (retrieve/list/redact).
// Any wire-shape drift from the official `BetaManagedAgentsMemoryStore` /
// `BetaManagedAgentsMemory` / `BetaManagedAgentsMemoryVersion` types surfaces as an
// SDK decode error.
//
// Run: (from e2e/)  node management_memory_stores_e2e.mjs
//
// Causal graph: store create -> versioned memory writes -> conditional update /
// redact / delete -> authoritative list and retrieval state.
// Decision table:
// | target | precondition | operation | observable behavior |
// | store | n/a | update/archive | metadata patch or terminal archive |
// | memory | matching hash | update/delete | new version or absence |
// | memory | stale hash | update | reject without mutation |
// | version | existing | redact | content becomes unavailable, history remains |
// | version list | exact lineage/operation/time | filter before paging |
// | version list | actor id absent from stored attribution | empty, never accept-and-drop |
// | version list | invalid operation/time | 400 before reading a page |
// Effects: valid rows expose exact CRUD/version/filter projections; stale or
// invalid rows reject without mutation. Constraints/invariant: the MemoryStore,
// Memory lineage, and immutable MemoryVersion history are the only authorities.
// Decision rules are the table rows above; every invalid partition is observed
// before pagination or mutation.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const MEMORY_BETA = 'agent-memory-2026-07-22';

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function json(baseUrl, method, route, body) {
  const response = await fetch(`${baseUrl}${route}`, {
    method,
    headers: {
      'anthropic-beta': MEMORY_BETA,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function main() {
  try {
    await withScenarioServer('management', 'mcp', 38144, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // Version-projection invariant: SDK calls omit `betas` so each exact
      // generated client owns its endpoint selector. The 0.105 anchor injects
      // the Managed beta; later anchors inject the dedicated Memory beta. Raw
      // negative probes below use the current selector explicitly.

      // -- Store CRUD --------------------------------------------------------
      const store = await client.beta.memoryStores.create({
        name: 'notes',
        description: 'my notes',
        metadata: { a: '1' },
      });
      assert.equal(store.type, 'memory_store');
      assert.equal(store.name, 'notes');
      pass('beta.memoryStores.create -> BetaManagedAgentsMemoryStore');

      const gotStore = await client.beta.memoryStores.retrieve(store.id);
      assert.equal(gotStore.id, store.id);

      const upStore = await client.beta.memoryStores.update(store.id, {
        description: 'updated notes',
        metadata: { a: null, b: '2' },
      });
      assert.equal(upStore.description, 'updated notes');
      assert.equal(upStore.metadata.b, '2');
      assert.ok(!('a' in upStore.metadata), 'null metadata patch removes the key');
      pass('beta.memoryStores.retrieve / update');

      const storeIds = (await drain(client.beta.memoryStores.list())).map((s) => s.id);
      assert.ok(storeIds.includes(store.id));
      // Store-list causal graph: lifecycle/time predicates narrow the durable
      // registry before PageCursor pagination. Inclusive bounds use the exact
      // second-precision timestamp returned to callers; TS null/empty query
      // spellings equal omission; malformed time fails before reading a page.
      const exactCreated = await drain(client.beta.memoryStores.list({
        'created_at[gte]': store.created_at,
        'created_at[lte]': store.created_at,
      }));
      assert.ok(exactCreated.some((candidate) => candidate.id === store.id));
      assert.deepEqual(
        await drain(client.beta.memoryStores.list({
          'created_at[gte]': '9999-12-31T23:59:59Z',
        })),
        [],
      );
      const nullFilters = await drain(client.beta.memoryStores.list({
        'created_at[gte]': null,
        'created_at[lte]': null,
        include_archived: null,
        limit: null,
        page: null,
      }));
      assert.ok(nullFilters.some((candidate) => candidate.id === store.id));
      pass('beta.memoryStores.list -> PageCursor<BetaManagedAgentsMemoryStore>');

      // Cause/effect boundary rule: C1 an Awaken-only MemoryStore behavior
      // route is addressed on the compatible surface -> E1 404 regardless of
      // method or payload. Recall/extraction now belong to ordinary Agent
      // plugin configuration, not a second resource policy authority.
      const configRoute = `/v1/memory_stores/${store.id}/config`;
      assert.equal((await json(baseUrl, 'GET', configRoute)).status, 404);
      assert.equal((await json(baseUrl, 'POST', configRoute, { expected_config_version: 1 })).status, 404);
      assert.equal((await json(baseUrl, 'GET', `/v1/memory_stores/${store.id}/config_versions/1`)).status, 404);
      assert.equal((await json(baseUrl, 'GET', '/v1/memory_stores/missing/config')).status, 404);
      pass('non-compatible MemoryStore behavior routes remain absent');

      // -- Memories ----------------------------------------------------------
      const mem = await client.beta.memoryStores.memories.create(store.id, {
        path: '/notes.md',
        content: 'first',
      });
      assert.equal(mem.type, 'memory');
      assert.equal(mem.path, '/notes.md');
      assert.equal(mem.content, null, 'create defaults to the basic projection');
      assert.ok(mem.content_sha256.length === 64, 'content_sha256 is a 64-char hex digest');
      assert.equal(mem.content_size_bytes, 5);
      pass('beta.memoryStores.memories.create -> BetaManagedAgentsMemory');

      const gotMem = await client.beta.memoryStores.memories.retrieve(mem.id, {
        memory_store_id: store.id,
      });
      assert.equal(gotMem.id, mem.id);
      assert.equal(gotMem.content, 'first', 'retrieve defaults to the full projection');
      const gotMemBasic = await client.beta.memoryStores.memories.retrieve(mem.id, {
        memory_store_id: store.id,
        view: 'basic',
      });
      assert.equal(gotMemBasic.content, null, 'explicit basic retrieve elides content');

      // Update with a stale precondition -> 409, then with the correct one.
      await assert.rejects(
        () =>
          client.beta.memoryStores.memories.update(mem.id, {
            memory_store_id: store.id,
            content: 'nope',
            precondition: { type: 'content_sha256', content_sha256: 'deadbeef'.repeat(8) },
          }),
        (err) => err.status === 409,
      );
      const upMem = await client.beta.memoryStores.memories.update(mem.id, {
        memory_store_id: store.id,
        view: 'full',
        content: 'second',
        precondition: { type: 'content_sha256', content_sha256: mem.content_sha256 },
      });
      assert.equal(upMem.content, 'second');
      assert.notEqual(upMem.memory_version_id, mem.memory_version_id, 'update mints a new version');
      pass('beta.memoryStores.memories.update -> precondition (409 stale, ok fresh)');

      const memIds = (await drain(client.beta.memoryStores.memories.list(store.id))).map(
        (m) => m.id,
      );
      assert.ok(memIds.includes(mem.id));
      pass('beta.memoryStores.memories.list');

      // A second memory in a subtree so path_prefix has something to filter.
      const subMem = await client.beta.memoryStores.memories.create(store.id, {
        path: '/archive/old.md',
        content: 'archived',
      });

      // path_prefix drills into the subtree, excluding /notes.md.
      const drilled = await drain(
        client.beta.memoryStores.memories.list(store.id, { path_prefix: '/archive/' }),
      );
      assert.deepEqual(
        drilled.map((m) => m.id),
        [subMem.id],
        'path_prefix returns only memories under the prefix',
      );

      // view=basic (also the list default) elides content; full must be explicit.
      const basic = await drain(
        client.beta.memoryStores.memories.list(store.id, { view: 'basic' }),
      );
      assert.ok(basic.length >= 2, 'view=basic still lists every memory');
      assert.ok(
        basic.every((m) => m.content === null || m.content === undefined),
        'view=basic omits content',
      );
      pass('beta.memoryStores.memories.list -> path_prefix + view=basic');

      // -- Versions ----------------------------------------------------------
      const versions = await drain(client.beta.memoryStores.memoryVersions.list(store.id));
      const ops = versions.map((v) => v.operation);
      assert.ok(ops.includes('created') && ops.includes('modified'), `ops: ${ops}`);
      assert.ok(versions.every((v) => v.content === null), 'version list defaults to basic');
      pass(`beta.memoryStores.memoryVersions.list -> ${versions.length} versions`);

      // Filter cause/effect decision table: each supplied condition narrows the
      // canonical immutable rows before cursor paging. Current scenario writes
      // have no actor attribution, so every actor-id filter truthfully returns
      // an empty page instead of accepting and dropping the query parameter.
      const createdLineage = await drain(client.beta.memoryStores.memoryVersions.list(store.id, {
        memory_id: mem.id,
        operation: 'created',
      }));
      assert.equal(createdLineage.length, 1);
      assert.equal(createdLineage[0].memory_id, mem.id);
      assert.equal(createdLineage[0].operation, 'created');
      const boundedLineage = await drain(client.beta.memoryStores.memoryVersions.list(store.id, {
        memory_id: mem.id,
        'created_at[gte]': createdLineage[0].created_at,
        'created_at[lte]': createdLineage[0].created_at,
      }));
      assert.ok(boundedLineage.length >= 1);
      const serviceAccountVersions = await drain(
        client.beta.memoryStores.memoryVersions.list(store.id, {
          service_account_id: 'svac_absent',
        }),
      );
      assert.deepEqual(serviceAccountVersions, []);
      for (const query of ['operation=unknown', 'created_at%5Bgte%5D=not-a-time']) {
        const rejectedFilter = await json(
          baseUrl,
          'GET',
          `/v1/memory_stores/${store.id}/memory_versions?${query}`,
        );
        assert.equal(rejectedFilter.status, 400, query);
      }
      pass('beta.memoryStores.memoryVersions.list filter decision table');

      const firstVer = versions[0];
      const gotVer = await client.beta.memoryStores.memoryVersions.retrieve(firstVer.id, {
        memory_store_id: store.id,
      });
      assert.equal(gotVer.id, firstVer.id);
      assert.equal(gotVer.content, 'first', 'version retrieve defaults to full');

      const redacted = await client.beta.memoryStores.memoryVersions.redact(firstVer.id, {
        memory_store_id: store.id,
      });
      assert.ok(redacted.redacted_at, 'redact stamps redacted_at');
      pass('beta.memoryStores.memoryVersions.retrieve / redact');

      // -- Delete memory + archive + delete store ----------------------------
      const delMem = await client.beta.memoryStores.memories.delete(mem.id, {
        memory_store_id: store.id,
      });
      assert.equal(delMem.type, 'memory_deleted');

      const archived = await client.beta.memoryStores.archive(store.id);
      assert.ok(archived.archived_at);
      assert.ok(
        !(await drain(client.beta.memoryStores.list())).some((candidate) => candidate.id === store.id),
        'archived stores are excluded by default',
      );
      assert.ok(
        (await drain(client.beta.memoryStores.list({ include_archived: true })))
          .some((candidate) => candidate.id === store.id),
        'include_archived restores archived stores',
      );
      const malformedStoreTime = await json(
        baseUrl,
        'GET',
        '/v1/memory_stores?beta=true&created_at%5Bgte%5D=not-a-time',
      );
      assert.equal(malformedStoreTime.status, 400, 'malformed store time fails closed');

      const delStore = await client.beta.memoryStores.delete(store.id);
      assert.equal(delStore.type, 'memory_store_deleted');
      assert.ok(
        !(await drain(client.beta.memoryStores.list({ include_archived: true })))
          .some((candidate) => candidate.id === store.id),
        'deleted stores never reappear as archived',
      );
      assert.equal(
        (
          await json(baseUrl, 'POST', configRoute, {
            expected_config_version: 2,
          })
        ).status,
        404,
        'resource lifecycle cannot make a removed behavior route visible',
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
