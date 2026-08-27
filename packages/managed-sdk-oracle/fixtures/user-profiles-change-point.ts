import LegacyAnthropic from '@anthropic-ai/sdk-user-profiles-legacy';
import CurrentAnthropic from '@anthropic-ai/sdk-current';

// Compile-only change-point contract. The 2026-03-24 response requires the
// legacy relationship field; the current response may instead identify the
// access model with access_type. If either SDK changes this boundary, tsc fails
// before a release can silently reinterpret the wire.
declare const legacyClient: LegacyAnthropic;
declare const legacyProfile: Awaited<ReturnType<typeof legacyClient.beta.userProfiles.retrieve>>;
const legacyRelationship: 'external' | 'resold' | 'internal' =
  legacyProfile.relationship;

declare const currentClient: CurrentAnthropic;
declare const currentProfile: Awaited<ReturnType<typeof currentClient.beta.userProfiles.retrieve>>;
const currentAccessType: 'application' | 'passthrough' | undefined =
  currentProfile.access_type;
const retainedRelationship: 'external' | 'resold' | 'internal' | undefined =
  currentProfile.relationship;

void [legacyRelationship, currentAccessType, retainedRelationship];
