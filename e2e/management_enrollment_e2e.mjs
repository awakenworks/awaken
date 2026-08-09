// Enrollment web flow (ADR-0050 G3): the controller mints a signed, expiring URL
// for a data subject; the end user opens an HTML consent page and accepts, which
// records the consent grant. Driven with plain fetch (Awaken extension).
//
// Run: (from e2e/)  node management_enrollment_e2e.mjs

import assert from 'node:assert/strict';
import { USER_PROFILES_BETA, withScenarioServer, pass } from './harness.mjs';

const PROFILE_HEADERS = { 'anthropic-beta': USER_PROFILES_BETA };

async function main() {
  await withScenarioServer('management', 'mcp', 38195, async (base) => {
    // Enrollment is a command on an existing User Profile; an unknown id must
    // fail 404 rather than minting a second implicit subject authority.
    const created = await fetch(`${base}/v1/user_profiles`, {
      method: 'POST',
      headers: { ...PROFILE_HEADERS, 'content-type': 'application/json' },
      body: JSON.stringify({ name: 'Enrollment E2E' }),
    });
    assert.equal(created.status, 200, `profile create status ${created.status}`);
    const { id } = await created.json();

    // Mint an enrollment URL for the subject.
    const mint = await fetch(
      `${base}/v1/user_profiles/${id}/enroll?purpose=telemetry_content`,
      { method: 'POST', headers: PROFILE_HEADERS },
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
    const consent = await (
      await fetch(`${base}/v1/user_profiles/${id}/consent`, { headers: PROFILE_HEADERS })
    ).json();
    assert.equal(consent.telemetry_content_ceiling, 'full', 'enrollment granted consent');
    assert.equal(consent.grants[0].version, 'enrollment', 'grant tagged from enrollment');
    pass('enrollment grant reflected in consent ceiling=full');
  });
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
