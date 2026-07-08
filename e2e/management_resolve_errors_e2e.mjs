// Resolver fail-closed paths: resolving an unauthored model / credential / pool
// is a client error, and a malformed body is 400. Drives config-resolver's
// resolve_inference error branches that the happy-path admin e2e does not reach.
// CI-safe.

import assert from 'node:assert/strict';
import { withScenarioServer, pass } from './harness.mjs';

async function req(base, uri, body, raw = false) {
  const res = await fetch(`${base}${uri}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: raw ? body : JSON.stringify(body),
  });
  return { status: res.status };
}

async function main() {
  await withScenarioServer('management', 'mcp', 38267, async (base) => {
    // Resolve an unauthored model → fail closed.
    let r = await req(base, '/v1/config/inference/resolve', {
      workspace_id: 'ws',
      model_id: 'ghost-model',
      binding: { type: 'exact', credential_source_id: 'ghost-cred' },
    });
    assert.ok(r.status >= 400 && r.status < 500, `resolve unauthored model -> 4xx (got ${r.status})`);

    // Resolve against a missing pool → fail closed.
    r = await req(base, '/v1/config/inference/resolve', {
      workspace_id: 'ws',
      model_id: 'ghost-model',
      binding: { type: 'pool', credential_pool_id: 'ghost-pool' },
    });
    assert.ok(r.status >= 400 && r.status < 500, `resolve missing pool -> 4xx (got ${r.status})`);
    pass('resolver fails closed on unauthored model / missing pool -> 4xx');

    // A malformed resolve body → 400.
    r = await req(base, '/v1/config/inference/resolve', '{ not valid json', true);
    assert.equal(r.status, 400, `malformed resolve body -> 400 (got ${r.status})`);
    pass('a malformed resolve body -> 400');

    console.log('E2E PASS: resolver fail-closed + malformed-body paths.');
  });
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
