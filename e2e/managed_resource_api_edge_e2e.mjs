// Resource-plane API error/edge paths on the session surface: a missing file 404
// (metadata + content), a missing memory store 404, a missing skill 404, and a
// malformed upload 4xx. Drives the files/memory-store/skills router error arms.
// CI-safe (echo model, no key).

import assert from 'node:assert/strict';
import { withRealServer, pass } from './harness.mjs';

async function req(base, method, uri, rawBody) {
  const res = await fetch(`${base}${uri}`, {
    method,
    headers: rawBody === undefined ? {} : { 'content-type': 'application/json' },
    body: rawBody,
  });
  return { status: res.status };
}

async function main() {
  await withRealServer('echo', 38265, async (base) => {
    for (const uri of [
      '/v1/files/file_ghost',
      '/v1/files/file_ghost/content',
      '/v1/memory_stores/mem_ghost',
      '/v1/skills/skill_ghost',
    ]) {
      const r = await req(base, 'GET', uri);
      assert.equal(r.status, 404, `${uri} -> 404 (got ${r.status})`);
    }
    pass('missing file/content/memory-store/skill -> 404');

    // A malformed skills upload body is a client error.
    const r = await req(base, 'POST', '/v1/skills', '{ not valid json');
    assert.ok(r.status >= 400 && r.status < 500, `malformed skill upload -> 4xx (got ${r.status})`);
    pass('a malformed resource-plane body is a client error');

    console.log('E2E PASS: resource-plane API error/edge paths (files/memory/skills).');
  });
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
