// Memory-store cross-tenant fencing (ADR-0053 / ADR-0051). A memory store is
// id-addressed and its id is the only capability, so a store authored under one
// workspace must not be readable/writable under another. The management build wraps
// the memory routes with workspace path addressing (`/v1/workspaces/{ws}/…` stamps
// the edge scope), and the ownership guard records the authoring scope on create and
// answers a cross-tenant access with 404 (no existence disclosure). A single-tenant
// (flat) request resolves to the default scope and is likewise fenced from a
// tenant-owned store.
//
// Run: (from e2e/)  node management_memory_tenancy_e2e.mjs

import assert from 'node:assert/strict';
import { withScenarioServer, pass } from './harness.mjs';

const BETA = 'managed-agents-2026-04-01';
const H = { 'content-type': 'application/json', 'anthropic-beta': BETA };

async function main() {
  await withScenarioServer('management', 'mcp', 38146, async (baseUrl) => {
    const ws = (tenant, p) => `${baseUrl}/v1/workspaces/${tenant}${p}`;

    // tenant-a authors a store and writes a memory into it.
    let r = await fetch(ws('tenant-a', '/memory_stores'), {
      method: 'POST',
      headers: H,
      body: JSON.stringify({ name: 'a-notes' }),
    });
    assert.equal(r.status, 200, 'tenant-a creates a store');
    const id = (await r.json()).id;
    assert.ok(id, 'the store id is minted server-side');

    r = await fetch(ws('tenant-a', `/memory_stores/${id}/memories`), {
      method: 'POST',
      headers: H,
      body: JSON.stringify({ path: '/secret.md', content: 'tenant-a only' }),
    });
    assert.equal(r.status, 200, 'tenant-a writes a memory');
    const memId = (await r.json()).id;
    pass('tenant-a authored a store + memory');

    // tenant-b is fenced from the store AND its subresources — 404, not 403.
    for (const [label, path] of [
      ['store', `/memory_stores/${id}`],
      ['memories list', `/memory_stores/${id}/memories`],
      ['memory', `/memory_stores/${id}/memories/${memId}`],
    ]) {
      const resp = await fetch(ws('tenant-b', path), { headers: H });
      assert.equal(resp.status, 404, `cross-tenant ${label} read is 404`);
    }
    // tenant-b cannot write into the fenced store either.
    r = await fetch(ws('tenant-b', `/memory_stores/${id}/memories`), {
      method: 'POST',
      headers: H,
      body: JSON.stringify({ path: '/inject.md', content: 'x' }),
    });
    assert.equal(r.status, 404, 'cross-tenant write is 404');
    pass('tenant-b is fenced from the store, its memories, and writes (404)');

    // The owner still reads its own store.
    r = await fetch(ws('tenant-a', `/memory_stores/${id}`), { headers: H });
    assert.equal(r.status, 200, 'the owner still reads its store');

    // An unscoped (flat / default-scope) request is fenced from a tenant-owned store.
    r = await fetch(`${baseUrl}/v1/memory_stores/${id}`, { headers: H });
    assert.equal(r.status, 404, 'default-scope access to a tenant store is fenced');
    pass('owner retains access; unscoped access is fenced');
  });
  pass('E2E PASS: memory-store cross-tenant fencing (ADR-0053 / ADR-0051)');
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
