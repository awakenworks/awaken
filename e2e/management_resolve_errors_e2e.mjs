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
  return { status: res.status, body: await res.text() };
}

async function main() {
  await withScenarioServer('management', 'mcp', Number(process.env.E2E_PORT), async (base) => {
    // Address the Workspace the edge resolved. A `workspace_id` naming another
    // one is refused 404, which is a different rule from the resolver fail-
    // closed paths below; asserting only "some 4xx" let that 404 stand in for
    // every one of them and hid a route that answered nothing else.
    const context = await fetch(`${base}/v1/config/workspace-context`);
    assert.equal(context.status, 200, 'workspace context');
    const ws = (await context.json()).workspace_id;
    assert.ok(ws, 'the edge resolved a Workspace');
    // Resolve an unauthored model → fail closed.
    let r = await req(base, '/v1/config/inference/resolve', {
      workspace_id: ws,
      target: { model_id: 'ghost-model' },
      binding: { type: 'exact', credential_source_id: 'ghost-cred' },
    });
    assert.equal(r.status, 404, `resolve unauthored model (got ${r.status} ${r.body})`);
    assert.match(r.body, /"code":"model_unresolved"/, `unauthored model names its own code: ${r.body}`);

    // Resolve against a missing pool → fail closed.
    r = await req(base, '/v1/config/inference/resolve', {
      workspace_id: ws,
      target: { model_id: 'ghost-model' },
      // `pool` is not a binding variant: the old body failed JSON
      // deserialization, so this rule never reached the resolver at all.
      binding: { type: 'one_of_credential_pool', credential_pool_id: 'ghost-pool' },
    });
    assert.equal(r.status, 404, `resolve missing pool (got ${r.status} ${r.body})`);
    assert.match(r.body, /"code":"model_unresolved"/, `missing pool names its own code: ${r.body}`);
    pass('resolver fails closed on unauthored model / missing pool -> 404 model_unresolved');

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
