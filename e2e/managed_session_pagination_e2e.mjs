// Session-list cursor pagination (session-operations "Listing sessions"), driven
// against the raw `GET /v1/sessions?limit=&page=` wire so the `PageCursor` shape
// (`{ data, has_more, next_page }`) and the after-id cursor semantics are asserted
// directly — the official SDK auto-follows `next_page`, which hides the mechanics.
//
// Design: create N sessions, then walk the list one row per page by cursor. Boundary
// values on `limit` (1 vs all); state-transition on the cursor (first page has
// next_page + has_more; the last page clears them); error-guess a fabricated cursor
// (awaken returns an empty terminal page, matching the tolerant client contract).
//
// Run: (from e2e/)  node managed_session_pagination_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38431);
const BETA = 'managed-agents-2026-04-01';
const BETAS = [BETA];

// Raw list request — the SDK would auto-paginate and swallow the cursor fields.
async function listPage(baseUrl, { limit, page } = {}) {
  const url = new URL(`${baseUrl}/v1/sessions`);
  if (limit != null) url.searchParams.set('limit', String(limit));
  if (page != null) url.searchParams.set('page', page);
  const res = await fetch(url, {
    headers: { 'anthropic-beta': BETA, 'x-api-key': 'e2e-dummy' },
  });
  assert.equal(res.status, 200, `list ${url.search} -> ${res.status}`);
  return res.json();
}

async function main() {
  await withRealServer('echo', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    // Three sessions in one scope (the raw GET shares the x-api-key -> same scope).
    const created = [];
    for (let i = 0; i < 3; i++) {
      const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      created.push(s.id);
    }
    assert.equal(new Set(created).size, 3, 'three distinct session ids');
    pass('created 3 sessions');

    // No limit -> single page with every row, terminal cursor.
    const all = await listPage(baseUrl);
    const allIds = all.data.map((s) => s.id);
    for (const id of created) assert.ok(allIds.includes(id), `full page missing ${id}`);
    assert.equal(all.has_more, false, 'full page has_more=false');
    assert.equal(all.next_page, null, 'full page next_page=null');
    pass('unpaged list returns every session, terminal cursor');

    // limit=1 -> walk the collection one row per page by after-id cursor.
    const walked = [];
    let cursor;
    for (let guard = 0; guard < 10; guard++) {
      const p = await listPage(baseUrl, { limit: 1, page: cursor });
      assert.ok(p.data.length <= 1, `limit=1 returned ${p.data.length} rows`);
      if (p.data.length === 0) break;
      walked.push(p.data[0].id);
      if (!p.has_more) {
        assert.equal(p.next_page, null, 'last page next_page=null');
        break;
      }
      assert.ok(p.next_page, 'a non-terminal page carries next_page');
      cursor = p.next_page;
    }
    assert.equal(walked.length, all.data.length, `cursor walk saw ${walked.length}, full page had ${all.data.length}`);
    assert.equal(new Set(walked).size, walked.length, 'cursor walk yields no duplicates');
    assert.deepEqual(walked, allIds, 'cursor walk preserves the full-page order');
    pass('limit=1 cursor walk covers every row exactly once, in order');

    // A fabricated cursor is a tolerant empty terminal page (not a 500/hang).
    const bogus = await listPage(baseUrl, { limit: 1, page: 'sesn_does_not_exist' });
    assert.equal(bogus.data.length, 0, 'unknown cursor -> empty page');
    assert.equal(bogus.has_more, false, 'unknown cursor -> has_more=false');
    pass('fabricated cursor -> empty terminal page');

    console.log('E2E PASS: session-list cursor pagination (limit/page, next_page/has_more, after-id).');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
