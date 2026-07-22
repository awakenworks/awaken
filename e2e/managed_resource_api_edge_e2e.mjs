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

async function createSession(base, resources) {
  const res = await fetch(`${base}/v1/sessions`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'anthropic-beta': 'managed-agents-2026-04-01',
    },
    body: JSON.stringify({ agent: 'assistant', resources }),
  });
  const text = await res.text();
  return { status: res.status, body: text ? JSON.parse(text) : null };
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

    // Cause/effect matrix at the Managed anti-corruption boundary. Each shape
    // would otherwise create an ungoverned or ambiguous mount; none may be
    // silently dropped from resources[].
    const malformedSessionResources = [
      { type: 'future_resource', id: 'opaque' },
      { type: 'file' },
      { type: 'file', file_id: 7 },
      { type: 'memory_store' },
      { type: 'memory_store', memory_store_id: 'm', access: 'owner' },
      { type: 'github_repository' },
      { type: 'github_repository', url: '/tmp/repo', checkout: { type: 'tag', name: 'v1' } },
    ];
    for (const [index, resource] of malformedSessionResources.entries()) {
      const denied = await createSession(base, [resource]);
      assert.equal(denied.status, 400, `${index}: ${JSON.stringify(denied.body)}`);
      assert.match(JSON.stringify(denied.body), /invalid resource/u, `${index}`);
    }
    pass('unsupported and malformed Session resource unions fail closed');

    console.log('E2E PASS: resource-plane API error/edge paths (files/memory/skills).');
  });
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
