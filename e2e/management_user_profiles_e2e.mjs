// The user-profiles family, driven by the official Anthropic TypeScript SDK
// (`client.beta.userProfiles.*`): create / retrieve / update / list / enrollment
// URL. Any wire-shape drift from the official `BetaUserProfile` /
// `BetaUserProfileEnrollmentURL` types surfaces as an SDK decode error.
//
// Run: (from e2e/)  node management_user_profiles_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  try {
    await withServer('management', 38136, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const profile = await client.beta.userProfiles.create({
        external_id: 'end-user-42',
        name: 'Acme Corp',
        relationship: 'resold',
        metadata: { tier: 'gold' },
        betas: BETAS,
      });
      assert.equal(profile.type, 'user_profile');
      assert.ok(profile.id.startsWith('uprof_'), `id: ${profile.id}`);
      assert.equal(profile.relationship, 'resold');
      assert.ok(profile.trust_grants && typeof profile.trust_grants === 'object');
      pass('beta.userProfiles.create -> BetaUserProfile');

      const got = await client.beta.userProfiles.retrieve(profile.id, { betas: BETAS });
      assert.equal(got.id, profile.id);
      pass('beta.userProfiles.retrieve -> BetaUserProfile');

      const updated = await client.beta.userProfiles.update(profile.id, {
        name: 'Acme Inc',
        metadata: { tier: '', region: 'us' }, // empty string removes `tier`
        betas: BETAS,
      });
      assert.equal(updated.name, 'Acme Inc');
      assert.equal(updated.metadata.region, 'us');
      assert.ok(!('tier' in updated.metadata), 'empty-string metadata value removes the key');
      pass('beta.userProfiles.update -> metadata merge (empty-string removes)');

      const ids = (await drain(client.beta.userProfiles.list({ betas: BETAS }))).map((p) => p.id);
      assert.ok(ids.includes(profile.id), 'list returns the profile');
      pass(`beta.userProfiles.list -> PageCursor<BetaUserProfile> (${ids.length})`);

      const enroll = await client.beta.userProfiles.createEnrollmentURL(profile.id, { betas: BETAS });
      assert.equal(enroll.type, 'enrollment_url');
      assert.ok(enroll.url.includes(profile.id), 'enrollment URL references the profile');
      assert.ok(enroll.expires_at, 'enrollment URL has an expiry');
      pass('beta.userProfiles.createEnrollmentURL -> BetaUserProfileEnrollmentURL');
    });

    console.log('E2E PASS: the user-profiles family round-trips through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
