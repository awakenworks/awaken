import assert from 'node:assert/strict';

export async function exerciseUserProfileChangePoint({
  profileID,
  expectedAccessType,
  legacyClient,
  currentClient,
}) {
  assert.ok(
    ['application', 'passthrough'].includes(expectedAccessType),
    'expectedAccessType must be application or passthrough',
  );
  const legacyProfile = await legacyClient.beta.userProfiles.retrieve(profileID, {
    betas: ['user-profiles-2026-03-24'],
  });
  const currentProfile = await currentClient.beta.userProfiles.retrieve(profileID, {
    betas: ['user-profiles-2026-08-18'],
  });
  assert.equal(legacyProfile.id, profileID, 'legacy anchor retrieves the fixture');
  assert.equal(currentProfile.id, profileID, 'current anchor retrieves the fixture');
  assert.equal(currentProfile.access_type, expectedAccessType, 'current access model');
  assert.equal(
    legacyProfile.relationship,
    expectedAccessType === 'application' ? 'external' : 'resold',
    'legacy relationship projects the same access model',
  );
  for (const field of ['external_id', 'name', 'metadata']) {
    assert.deepEqual(legacyProfile[field], currentProfile[field], `${field} remains stable`);
  }
}
