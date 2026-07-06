// Error/edge paths on the /v1/config authoring surface: missing rows 404, malformed
// bodies 400, invalid ids 422, dangling references. Drives the admin router's error
// arms that the happy-path admin e2e does not reach. Deterministic, CI-safe.

import assert from 'node:assert/strict';
import { withServer, pass } from './harness.mjs';

async function req(base, method, uri, rawBody, json = true) {
  const res = await fetch(`${base}${uri}`, {
    method,
    headers: rawBody === undefined ? {} : { 'content-type': 'application/json' },
    body: rawBody,
  });
  const text = await res.text();
  return { status: res.status, text };
}

async function main() {
  await withServer('management', 38251, async (base) => {
    // Missing rows across namespaces → 404.
    for (const uri of [
      '/v1/config/providers/ghost',
      '/v1/config/endpoints/ghost',
      '/v1/config/mcp-servers/ghost',
      '/v1/config/projects/ghost',
      '/v1/config/projects/ghost/agents/x/mcp',
    ]) {
      const r = await req(base, 'GET', uri);
      assert.equal(r.status, 404, `${uri} -> 404 (got ${r.status})`);
    }
    pass('missing config rows across namespaces -> 404');

    // Malformed JSON body → 400 (decode failure).
    let r = await req(base, 'PUT', '/v1/config/providers/p', '{ not valid json');
    assert.equal(r.status, 400, `malformed provider body -> 400 (got ${r.status})`);
    r = await req(base, 'PUT', '/v1/config/projects/p', '{ bad');
    assert.equal(r.status, 400, `malformed project body -> 400 (got ${r.status})`);
    pass('malformed JSON bodies -> 400');

    // An invalid project id (not a DNS-safe slug) → 422.
    r = await req(base, 'PUT', '/v1/config/projects/Not_A_Slug',
      JSON.stringify({ id: 'x', workspace_id: 'ws', display_name: 'bad', version: 1 }));
    assert.equal(r.status, 422, `invalid project id -> 422 (got ${r.status})`);
    pass('invalid project id -> 422');

    // A missing required field → 400/422 (deserialize/validate).
    r = await req(base, 'PUT', '/v1/config/projects/ok-id',
      JSON.stringify({ id: 'ok-id', display_name: 'no workspace' }));
    assert.ok([400, 422].includes(r.status), `project missing workspace_id -> 4xx (got ${r.status})`);
    pass('a body missing a required field is rejected');

    console.log('E2E PASS: /v1/config error + edge paths (404 / 400 / 422).');
  });
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
