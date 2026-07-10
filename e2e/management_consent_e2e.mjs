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

    // A subject with no eval_recording consent still caps that purpose (proven by
    // a fresh subject reading structured by default is covered by the unit tests;
    // here we assert the granted purpose did not leak to erasure removing content).
    const erased = await fetch(`${baseUrl}/v1/user_profiles/${id}/erasure`, {
      method: 'POST',
    });
    assert.equal(erased.status, 200, `erasure status ${erased.status}`);
    // After erasure the subject record is gone → consent reads 404 again.
    const afterErase = await fetch(`${baseUrl}/v1/user_profiles/${id}/consent`);
    assert.equal(afterErase.status, 404, 'erasure removed the subject record');
    pass('erasure removes the subject → consent 404 (withdrawal path)');
  });
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
