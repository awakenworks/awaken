// Consent read/write loop (ADR-0050): the Awaken-neutral consent grant on a data
// subject (NOT the Anthropic `trust_grants` field). `POST /v1/user_profiles/:id/
// consent` records a Granted grant; `GET` reflects the grants + the resolved
// telemetry capture ceiling. Driven with plain fetch (Awaken extension).
//
// Run: (from e2e/)  node management_consent_e2e.mjs

import assert from 'node:assert/strict';
import { withScenarioServer, pass } from './harness.mjs';

async function main() {
  await withScenarioServer('management', 'mcp', 38192, async (baseUrl) => {
    const id = 'dsub_consent_e2e';

    // Unknown subject → 404 on read.
    const missing = await fetch(`${baseUrl}/v1/user_profiles/${id}/consent`);
    assert.equal(missing.status, 404, `unknown subject status ${missing.status}`);
    pass('GET consent (unknown subject) -> 404');

    // Grant telemetry_content consent.
    const granted = await fetch(`${baseUrl}/v1/user_profiles/${id}/consent`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ purpose: 'telemetry_content', version: 'v1' }),
    });
    assert.equal(granted.status, 200, `grant status ${granted.status}`);
    const gbody = await granted.json();
    assert.equal(gbody.grants.length, 1, 'one grant recorded');
    assert.equal(gbody.grants[0].purpose, 'telemetry_content');
    assert.equal(gbody.grants[0].status, 'granted');
    // Consent opens the capture ceiling to full.
    assert.equal(gbody.telemetry_content_ceiling, 'full', 'ceiling is full after grant');
    pass('POST consent -> grant recorded, ceiling=full');

    // The grant persists and reads back.
    const read = await fetch(`${baseUrl}/v1/user_profiles/${id}/consent`);
    assert.equal(read.status, 200);
    const rbody = await read.json();
    assert.equal(rbody.telemetry_content_ceiling, 'full', 'read-back ceiling=full');
    pass('GET consent -> grant persisted, ceiling=full');

    // Erasure removes CONTENT but RETAINS the consent record as accountability
    // proof (Art. 5(2)/7(1)): the grant is withdrawn, not deleted, and the
    // ceiling drops back to structured.
    const erased = await fetch(`${baseUrl}/v1/user_profiles/${id}/erasure`, {
      method: 'POST',
    });
    assert.equal(erased.status, 200, `erasure status ${erased.status}`);
    const afterErase = await (
      await fetch(`${baseUrl}/v1/user_profiles/${id}/consent`)
    ).json();
    assert.equal(afterErase.telemetry_content_ceiling, 'structured', 'consent withdrawn');
    assert.equal(afterErase.grants[0].status, 'withdrawn', 'grant retained as audit (withdrawn)');
    pass('erasure withdraws consent but RETAINS the audit record (Art. 5(2)/7(1))');
  });
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
