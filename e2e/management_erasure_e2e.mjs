// GDPR right-to-erasure (ADR-0050 Slice 10): the Awaken extension
// `POST /v1/user_profiles/:id/erasure` over the neutral data-subject resolver.
// Not an SDK method, so driven with plain fetch. Asserts the endpoint is live on
// the real server binary and returns an `ErasureReceipt` shape.
//
// Run: (from e2e/)  node management_erasure_e2e.mjs

import assert from 'node:assert/strict';
import { withScenarioServer, pass } from './harness.mjs';

async function main() {
  await withScenarioServer('management', 'mcp', 38191, async (baseUrl) => {
    const res = await fetch(`${baseUrl}/v1/user_profiles/dsub_e2e/erasure`, {
      method: 'POST',
    });
    assert.equal(res.status, 200, `status ${res.status}`);
    const body = await res.json();
    assert.equal(typeof body.records_removed, 'number', 'receipt has records_removed');
    // An unknown subject has no captured content: erasure is a no-op receipt.
    assert.equal(body.records_removed, 0, 'no content removed for an unknown subject');
    pass('POST /v1/user_profiles/:id/erasure -> ErasureReceipt { records_removed }');

    // A second erasure is idempotent (still 200 + receipt).
    const again = await fetch(`${baseUrl}/v1/user_profiles/dsub_e2e/erasure`, {
      method: 'POST',
    });
    assert.equal(again.status, 200, `repeat status ${again.status}`);
    pass('erasure is idempotent');
  });
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
