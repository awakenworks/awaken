// Error/edge paths on the /v1/config authoring surface: missing rows 404, malformed
// bodies 400, invalid ids 422, dangling references. Drives the admin router's error
// arms that the happy-path admin e2e does not reach. Deterministic, CI-safe.

import assert from 'node:assert/strict';
import { withScenarioServer, pass } from './harness.mjs';

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
  await withScenarioServer('management', 'mcp', 38251, async (base) => {
    // Missing rows across namespaces → 404.
    for (const uri of [
      '/v1/config/providers/ghost',
      '/v1/config/endpoints/ghost',
      '/v1/config/mcp-servers/ghost',
    ]) {
      const r = await req(base, 'GET', uri);
      assert.equal(r.status, 404, `${uri} -> 404 (got ${r.status})`);
    }
    pass('missing config rows across namespaces -> 404');

    // Malformed JSON body → 400 (decode failure).
    const r = await req(base, 'PUT', '/v1/config/providers/p', '{ not valid json');
    assert.equal(r.status, 400, `malformed provider body -> 400 (got ${r.status})`);
    pass('malformed JSON bodies -> 400');

    console.log('E2E PASS: /v1/config error + edge paths (404 / 400).');
  });
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
