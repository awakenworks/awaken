// The user-profiles family, driven by the official Anthropic TypeScript SDK
// (`client.beta.userProfiles.*`): create / retrieve / update / list / enrollment
// URL. Any wire-shape drift from the official `BetaUserProfile` /
// `BetaUserProfileEnrollmentURL` types surfaces as an SDK decode error.
//
// Run: (from e2e/)  node management_user_profiles_e2e.mjs
//
// Causal graph: profile create -> metadata patch -> enrollment request -> scoped
// expiring URL; missing profiles fail before any enrollment side effect.
// Decision table:
// | profile | patch/enrollment | observable behavior |
// | present | empty-string metadata | key removed |
// | present | nullable fields = null | fields clear; relationship resets external |
// | access_type only | application/passthrough | relationship derives external/resold |
// | legacy relationship only | any supported relationship | access_type omitted |
// | both vocabularies | equivalent/conflicting | accept one aggregate / reject pre-write |
// | generated beta | 2026-03-24 / 2026-08-18 | both enter this one family |
// | beta absent | any request | 400, no profile created |
// | present | enrollment | URL contains profile id and expiry |
// | any | unknown request field | 400; profile state unchanged |
// | missing | read/update/enrollment | 404, no profile created |
// Effects: accepted rows project one canonical profile/enrollment DTO; rejected
// rows preserve prior state and create no side effect. Constraints/invariant:
// access_type and legacy relationship are two wire vocabularies for one profile
// aggregate, not parallel authorities. Decision rules are the table rows above.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { USER_PROFILES_BETA, withScenarioServer, pass } from './harness.mjs';

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  try {
    await withScenarioServer('management', 'mcp', 38136, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const missingBeta = await fetch(`${baseUrl}/v1/user_profiles`, {
        method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}',
      });
      assert.equal(missingBeta.status, 400);
      const latestBeta = await fetch(`${baseUrl}/v1/user_profiles`, {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          'anthropic-beta': 'user-profiles-2026-08-18',
        },
        body: JSON.stringify({ access_type: 'application' }),
      });
      assert.equal(latestBeta.status, 200);
      assert.equal((await latestBeta.json()).access_type, 'application');

      const profile = await client.beta.userProfiles.create({
        external_id: 'end-user-42',
        name: 'Acme Corp',
        relationship: 'resold',
        metadata: { tier: 'gold' },
      });
      assert.equal(profile.type, 'user_profile');
      assert.ok(profile.id.startsWith('uprof_'), `id: ${profile.id}`);
      assert.equal(profile.relationship, 'resold');
      assert.ok(profile.trust_grants && typeof profile.trust_grants === 'object');
      pass('beta.userProfiles.create -> BetaUserProfile');

      // Access vocabulary cause/effect rules: 0.120 `access_type` and legacy
      // `relationship` enter one profile aggregate. An access-only write derives
      // the matching legacy projection; a legacy-only write clears the access
      // projection; equivalent dual input is accepted and conflicting dual input
      // fails before revision/state mutation.
      const accessProfile = await client.beta.userProfiles.create({
        access_type: 'passthrough',
        name: 'Resold Company',
      });
      assert.equal(accessProfile.access_type, 'passthrough');
      assert.equal(accessProfile.relationship, 'resold');
      const applicationProfile = await client.beta.userProfiles.update(accessProfile.id, {
        access_type: 'application',
      });
      assert.equal(applicationProfile.access_type, 'application');
      assert.equal(applicationProfile.relationship, 'external');
      const legacyProfile = await client.beta.userProfiles.update(accessProfile.id, {
        relationship: 'resold',
      });
      assert.equal(legacyProfile.access_type, undefined);
      assert.equal(legacyProfile.relationship, 'resold');
      const conflictingAccess = await fetch(`${baseUrl}/v1/user_profiles/${accessProfile.id}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'anthropic-beta': USER_PROFILES_BETA },
        body: JSON.stringify({ access_type: 'application', relationship: 'resold' }),
      });
      assert.equal(conflictingAccess.status, 400);
      const conflictingNullRelationship = await fetch(
        `${baseUrl}/v1/user_profiles/${accessProfile.id}`,
        {
          method: 'POST',
          headers: { 'content-type': 'application/json', 'anthropic-beta': USER_PROFILES_BETA },
          body: JSON.stringify({ access_type: 'passthrough', relationship: null }),
        },
      );
      assert.equal(conflictingNullRelationship.status, 400);
      const afterConflictingAccess = await client.beta.userProfiles.retrieve(accessProfile.id);
      assert.equal(afterConflictingAccess.access_type, undefined);
      assert.equal(afterConflictingAccess.relationship, 'resold');
      for (const body of [{ access_type: null }, { relationship: null }]) {
        const rejectedCreateNull = await fetch(`${baseUrl}/v1/user_profiles`, {
          method: 'POST',
          headers: { 'content-type': 'application/json', 'anthropic-beta': USER_PROFILES_BETA },
          body: JSON.stringify(body),
        });
        assert.equal(rejectedCreateNull.status, 400, JSON.stringify(body));
      }
      pass('beta.userProfiles access_type/relationship decision table');

      const got = await client.beta.userProfiles.retrieve(profile.id);
      assert.equal(got.id, profile.id);
      pass('beta.userProfiles.retrieve -> BetaUserProfile');

      const updated = await client.beta.userProfiles.update(profile.id, {
        name: 'Acme Inc',
        metadata: { tier: '', region: 'us' }, // empty string removes `tier`
      });
      assert.equal(updated.name, 'Acme Inc');
      assert.equal(updated.metadata.region, 'us');
      assert.ok(!('tier' in updated.metadata), 'empty-string metadata value removes the key');
      pass('beta.userProfiles.update -> metadata merge (empty-string removes)');

      const rejectedUpdate = await fetch(`${baseUrl}/v1/user_profiles/${profile.id}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'anthropic-beta': USER_PROFILES_BETA },
        body: JSON.stringify({ parallel_profile_config: true }),
      });
      assert.equal(rejectedUpdate.status, 400);
      const afterRejectedUpdate = await client.beta.userProfiles.retrieve(profile.id);
      assert.equal(afterRejectedUpdate.name, 'Acme Inc', 'rejected patch has no state effect');

      const cleared = await client.beta.userProfiles.update(profile.id, {
        external_id: null,
        name: null,
        relationship: null,
      });
      assert.equal(cleared.external_id, undefined);
      assert.equal(cleared.name, undefined);
      assert.equal(cleared.relationship, 'external');
      assert.deepEqual(cleared.trust_grants, {});
      pass('beta.userProfiles.update -> explicit null is distinct from omission');

      const ids = (await drain(client.beta.userProfiles.list())).map((p) => p.id);
      assert.ok(ids.includes(profile.id), 'list returns the profile');
      pass(`beta.userProfiles.list -> PageCursor<BetaUserProfile> (${ids.length})`);

      const enroll = await client.beta.userProfiles.createEnrollmentURL(profile.id);
      assert.equal(enroll.type, 'enrollment_url');
      assert.ok(enroll.url.startsWith('/enroll/'), 'enrollment URL uses the public handoff route');
      assert.ok(
        !enroll.url.includes(profile.id),
        'the signed handoff keeps the profile id out of the visible URL path',
      );
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
