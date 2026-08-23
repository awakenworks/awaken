// Session-list contract from session-operations, exercised through the pinned
// official SDK's BidirectionalPageCursor. Raw HTTP remains only as the
// independent malformed-cursor oracle.
//
// Cause/effect graph: order + filters + lifecycle -> stable ordered candidate set;
// limit + opaque direction-bound cursor -> page and next/previous transitions;
// malformed/order-mismatched cursor -> atomic 400.
//
// Decision table:
// | Rule | order/cursor | filters/lifecycle | Effect |
// | L1 | omitted / none | active | newest-first page; prev=null |
// | L2 | asc / next | matching | forward walk, no gaps/duplicates |
// | L3 | asc / prev | matching | return to the preceding page |
// | L4 | cursor order differs | any | 400, no list projection |
// | L5 | malformed cursor | any | 400 |
// | L6 | none | agent/version/status/time/resource/deployment | intersection |
// | L7 | none | archived, include_archived omitted/true | excluded/included |
//
// Causes: sort order, page direction, limit, filter set, and archive state.
// Constraints: cursor embeds the order; statuses and RFC3339 times are validated;
// other filters and limit may change without invalidating the cursor.
// Effects: exact bidirectional page fields, deterministic membership/order, and
// 400 for caller-fabricated or order-conflicting cursors.
// Decision rules: L1-L7.
//
// Run: (from e2e/)  node managed_session_pagination_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38431);
const BETA = 'managed-agents-2026-04-01';
const BETAS = [BETA];

// Raw negative wire oracle. Positive single-page reads use the SDK Page's
// public data/next_page/prev_page fields below.
async function rejectRawListPage(baseUrl, params = {}) {
  const url = new URL(`${baseUrl}/v1/sessions`);
  for (const [key, value] of Object.entries(params)) {
    for (const item of Array.isArray(value) ? value : [value]) {
      if (item != null) url.searchParams.append(key, String(item));
    }
  }
  const res = await fetch(url, {
    headers: { 'anthropic-beta': BETA, 'x-api-key': 'e2e-dummy' },
  });
  assert.equal(res.status, 400, `list ${url.search} -> ${res.status}`);
  await res.json();
}

async function main() {
  await withRealServer('echo', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, maxRetries: 0 });

    // Three sessions in one SDK-owned x-api-key scope. The raw negative oracle
    // below uses the same key solely to isolate cursor validation.
    const created = [];
    for (let i = 0; i < 3; i++) {
      const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      created.push(s.id);
    }
    assert.equal(new Set(created).size, 3, 'three distinct session ids');
    pass('created 3 sessions');

    // L1: no limit/order uses official newest-first and exact bidirectional shape.
    const all = await client.beta.sessions.list({ betas: BETAS });
    const allIds = all.data.map((s) => s.id);
    for (const id of created) assert.ok(allIds.includes(id), `full page missing ${id}`);
    assert.equal(all.next_page, null, 'full page next_page=null');
    assert.equal(all.prev_page, null, 'first page prev_page=null');
    assert.deepEqual(allIds, [...allIds].sort().reverse(), 'default order is newest-first (id tie-break)');
    pass('L1 default list is newest-first and returns the bidirectional page shape');

    // L2: asc + limit=1 walks via opaque next cursors with no gaps/duplicates.
    const walked = [];
    let cursor;
    let secondPage;
    for (let guard = 0; guard < 10; guard++) {
      const p = await client.beta.sessions.list({
        limit: 1, order: 'asc', page: cursor, betas: BETAS,
      });
      assert.ok(p.data.length <= 1, `limit=1 returned ${p.data.length} rows`);
      if (p.data.length === 0) break;
      walked.push(p.data[0].id);
      if (walked.length === 2) secondPage = p;
      if (!p.next_page) {
        assert.equal(p.next_page, null, 'last page next_page=null');
        break;
      }
      assert.ok(p.next_page, 'a non-terminal page carries next_page');
      assert.ok(!p.next_page.includes(p.data[0].id), 'cursor is opaque, not a raw row id');
      cursor = p.next_page;
    }
    assert.equal(walked.length, all.data.length, `cursor walk saw ${walked.length}, full page had ${all.data.length}`);
    assert.equal(new Set(walked).size, walked.length, 'cursor walk yields no duplicates');
    assert.deepEqual(walked, [...allIds].reverse(), 'asc walk reverses default desc order');
    pass('L2 next-page walk covers every row exactly once in requested order');

    // L3: the second page's prev cursor returns the first page. The limit may change
    // when a cursor is reused; here it remains one to make the inverse exact.
    assert.ok(secondPage?.prev_page, 'a non-first page carries prev_page');
    const previous = await client.beta.sessions.list({
      limit: 1, order: 'asc', page: secondPage.prev_page, betas: BETAS,
    });
    assert.equal(previous.data[0].id, walked[0], 'prev cursor returns the preceding page');
    pass('L3 prev_page navigates back to the preceding page');

    // L4/L5: an opaque cursor is order-bound and malformed cursors are caller errors.
    await rejectRawListPage(baseUrl, { limit: 1, order: 'desc', page: secondPage.prev_page });
    await rejectRawListPage(baseUrl, { limit: 1, page: 'sesn_does_not_exist' });
    pass('L4/L5 order-conflicting and fabricated cursors reject with 400');

    // L6: filters intersect. The SDK owns the official `statuses` array's wire
    // serialization; this test observes only the public Page contract.
    const createdAt = all.data[0].created_at;
    const filtered = await client.beta.sessions.list({
      agent_id: 'assistant', agent_version: 1, statuses: ['idle'],
      'created_at[gte]': createdAt, 'created_at[lte]': createdAt,
      betas: BETAS,
    });
    assert.equal(filtered.data.length, created.length, 'matching filters retain all three sessions');
    const wrongStatus = await client.beta.sessions.list({ statuses: ['running'], betas: BETAS });
    assert.equal(wrongStatus.data.length, 0, 'nonmatching status removes every idle session');
    const changedFilterPage = await client.beta.sessions.list({
      limit: 1, order: 'asc', page: secondPage.next_page, statuses: ['running'], betas: BETAS,
    });
    assert.deepEqual(changedFilterPage.data, [], 'cursor remains safe when another filter changes');
    const wrongDeployment = await client.beta.sessions.list({
      deployment_id: 'depl_missing', betas: BETAS,
    });
    assert.equal(wrongDeployment.data.length, 0, 'deployment filter is enforced');
    const wrongMemory = await client.beta.sessions.list({
      memory_store_id: 'memstore_missing', betas: BETAS,
    });
    assert.equal(wrongMemory.data.length, 0, 'memory-store filter is enforced');
    pass('L6 agent/version/status/time/resource/deployment filters intersect');

    // L7: archive is excluded by default and restored by include_archived=true.
    await client.beta.sessions.archive(created[0], { betas: BETAS });
    const activeOnly = await client.beta.sessions.list({ betas: BETAS });
    assert.ok(!activeOnly.data.some((session) => session.id === created[0]), 'archived excluded by default');
    const includingArchived = await client.beta.sessions.list({ include_archived: true, betas: BETAS });
    assert.ok(includingArchived.data.some((session) => session.id === created[0]), 'archived included explicitly');
    pass('L7 include_archived controls archived membership');

    // Official SDK parses prev_page and auto-follows next_page without a compatibility shim.
    const sdkRows = [];
    for await (const session of client.beta.sessions.list({ limit: 1, order: 'asc', betas: BETAS })) {
      sdkRows.push(session.id);
    }
    assert.deepEqual(sdkRows, walked.filter((id) => id !== created[0]), 'SDK auto-pagination sees active rows');
    pass('official BidirectionalPageCursor auto-pagination works unmodified');

    console.log('E2E PASS: session-list filters and bidirectional opaque pagination.');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
