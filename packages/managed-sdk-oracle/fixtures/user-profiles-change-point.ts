import LegacyAnthropic from '@anthropic-ai/sdk-user-profiles-legacy';
import CurrentAnthropic from '@anthropic-ai/sdk-current';

// Compile-only change-point contract. The 2026-03-24 response requires the
// legacy relationship field; the current response identifies the access model
// with access_type and excludes relationship. If either SDK changes this boundary, tsc fails
// before a release can silently reinterpret the wire.
declare const legacyClient: LegacyAnthropic;
declare const legacyProfile: Awaited<ReturnType<typeof legacyClient.beta.userProfiles.retrieve>>;
const legacyRelationship: 'external' | 'resold' | 'internal' =
  legacyProfile.relationship;

declare const currentClient: CurrentAnthropic;
declare const currentProfile: Awaited<ReturnType<typeof currentClient.beta.userProfiles.retrieve>>;
const currentAccessType: 'application' | 'passthrough' | undefined =
  currentProfile.access_type;
type CurrentProfile = typeof currentProfile;
const currentRelationshipIsAbsent:
  'relationship' extends keyof CurrentProfile ? never : true = true;

void [legacyRelationship, currentAccessType, currentRelationshipIsAbsent];
