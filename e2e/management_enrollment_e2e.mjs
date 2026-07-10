// Enrollment web flow (ADR-0050 G3): the controller mints a signed, expiring URL
// for a data subject; the end user opens an HTML consent page and accepts, which
// records the consent grant. Driven with plain fetch (Awaken extension).
//
// Run: (from e2e/)  node management_enrollment_e2e.mjs

import assert from 'node:assert/strict';
import { withScenarioServer, pass } from './harness.mjs';

async function main() {
  await withScenarioServer('management', 'mcp', 38195, async (base) => {
    const id = 'dsub_enroll';

    // Mint an enrollment URL for the subject.
    const mint = await fetch(
      `${base}/v1/user_profiles/${id}/enroll?purpose=telemetry_content`,
      { method: 'POST' },
    );
    assert.equal(mint.status, 200, `mint status ${mint.status}`);
    const { type, url } = await mint.json();
    assert.equal(type, 'enrollment_url');
    assert.ok(url.startsWith('/enroll/'), `url: ${url}`);
    pass('POST /enroll -> signed enrollment_url');

    // The end user opens the consent page.
    const page = await fetch(`${base}${url}`);
    assert.equal(page.status, 200);
    const html = await page.text();
    assert.ok(html.includes('Accept'), 'consent page renders an Accept action');
    assert.ok(html.includes('TelemetryContent'), 'page names the purpose');
    pass('GET /enroll/:token -> HTML consent page');

    // A tampered token is rejected by the page.
    const bad = await fetch(`${base}/enroll/not.a.valid.token`);
    assert.ok((await bad.text()).includes('invalid or expired'), 'tampered token rejected');
    pass('tampered token -> invalid page');

    // The end user accepts → grant recorded.
    const grant = await fetch(`${base}${url}/grant`, { method: 'POST' });
    assert.equal(grant.status, 200, `grant status ${grant.status}`);
    assert.ok((await grant.text()).includes('recorded'), 'grant confirmation');
    pass('POST /enroll/:token/grant -> consent recorded');

    // Consent now reads back as granted (ceiling = full).
    const consent = await (await fetch(`${base}/v1/user_profiles/${id}/consent`)).json();
    assert.equal(consent.telemetry_content_ceiling, 'full', 'enrollment granted consent');
    assert.equal(consent.grants[0].version, 'enrollment', 'grant tagged from enrollment');
    pass('enrollment grant reflected in consent ceiling=full');
  });
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
