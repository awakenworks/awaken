// GDPR right-to-erasure (ADR-0050 Slice 10): the Awaken extension
// `POST /v1/user_profiles/:id/erasure` over the neutral data-subject resolver.
// Not an SDK method, so driven with plain fetch. Asserts the endpoint is live on
// the real server binary and returns an `ErasureReceipt` shape.
//
// Run: (from e2e/)  node management_erasure_e2e.mjs

import assert from 'node:assert/strict';
import { USER_PROFILES_BETA, withScenarioServer, pass } from './harness.mjs';

const PROFILE_HEADERS = { 'anthropic-beta': USER_PROFILES_BETA };

async function main() {
  await withScenarioServer('management', 'mcp', 38191, async (baseUrl) => {
    const unknown = await fetch(`${baseUrl}/v1/user_profiles/dsub_missing/erasure`, {
      method: 'POST',
      headers: PROFILE_HEADERS,
    });
    assert.equal(unknown.status, 404, 'an unknown aggregate has no erasure authority');

    // Consent ingress creates the neutral Data Subject aggregate without adding
    // captured content, so erasure can prove the zero-row receipt and replay.
    const subject = await fetch(`${baseUrl}/v1/user_profiles/dsub_e2e/consent`, {
      method: 'POST',
      headers: { ...PROFILE_HEADERS, 'content-type': 'application/json' },
      body: JSON.stringify({ purpose: 'telemetry_content', version: 'v1' }),
    });
    assert.equal(subject.status, 200, `subject setup status ${subject.status}`);

    // Gate/FMECA rule: absent beta is rejected before erasure; canonical beta
    // reaches the idempotent application command, including an unknown subject.
    const res = await fetch(`${baseUrl}/v1/user_profiles/dsub_e2e/erasure`, {
      method: 'POST',
      headers: PROFILE_HEADERS,
    });
    assert.equal(res.status, 200, `status ${res.status}`);
    const body = await res.json();
    assert.equal(typeof body.records_removed, 'number', 'receipt has records_removed');
    // An existing subject with no captured content returns a zero-effect receipt.
    assert.equal(body.records_removed, 0, 'no content removed for an empty subject');
    pass('POST /v1/user_profiles/:id/erasure -> ErasureReceipt { records_removed }');

    // A second erasure is idempotent (still 200 + receipt).
    const again = await fetch(`${baseUrl}/v1/user_profiles/dsub_e2e/erasure`, {
      method: 'POST',
      headers: PROFILE_HEADERS,
    });
    assert.equal(again.status, 200, `repeat status ${again.status}`);
    pass('erasure is idempotent');
  });
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
