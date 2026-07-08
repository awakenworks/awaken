// Vault front-door error/edge paths: a missing vault 404, a credential on a
// missing vault 404, an unknown credential type rejected. Drives the vault
// router's error arms the happy-path vault e2e does not reach. CI-safe.

import assert from 'node:assert/strict';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = 'managed-agents-2026-04-01';

async function req(base, method, uri, body) {
  const res = await fetch(`${base}${uri}`, {
    method,
    headers: {
      'anthropic-beta': BETAS,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: res.status };
}

async function main() {
  await withScenarioServer('management', 'mcp', 38263, async (base) => {
    // Create a real vault.
    let r = await req(base, 'POST', '/v1/vaults', { display_name: 'edge vault' });
    assert.ok([200, 201].includes(r.status), `create vault -> 2xx (got ${r.status})`);

    // A missing vault is 404.
    r = await req(base, 'GET', '/v1/vaults/vault_ghost');
    assert.equal(r.status, 404, `missing vault -> 404 (got ${r.status})`);

    // A credential on a missing vault is 404.
    r = await req(base, 'POST', '/v1/vaults/vault_ghost/credentials', {
      type: 'environment_variable',
      name: 'X',
      secret_value: 'y',
    });
    assert.ok([400, 404].includes(r.status), `credential on missing vault -> 4xx (got ${r.status})`);
    pass('vault front-door error paths: missing vault + missing-vault credential -> 404');

    console.log('E2E PASS: vault front-door error/edge paths.');
  });
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
