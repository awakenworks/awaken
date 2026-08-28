import assert from 'node:assert/strict';

const REFERENCE_ENVIRONMENT = Object.freeze({
  baseURL: 'ANTHROPIC_MANAGED_REFERENCE_BASE_URL',
  apiKey: 'ANTHROPIC_MANAGED_REFERENCE_API_KEY', // awaken-allow: secret
  tunnelAccessToken: 'ANTHROPIC_MANAGED_REFERENCE_TUNNEL_ACCESS_TOKEN', // awaken-allow: secret
  agent: 'ANTHROPIC_MANAGED_REFERENCE_AGENT_ID',
  environmentId: 'ANTHROPIC_MANAGED_REFERENCE_ENVIRONMENT_ID',
  workspaceId: 'ANTHROPIC_MANAGED_REFERENCE_WORKSPACE_ID',
  userProfileId: 'ANTHROPIC_MANAGED_REFERENCE_USER_PROFILE_ID',
  userProfileAccessType: 'ANTHROPIC_MANAGED_REFERENCE_USER_PROFILE_ACCESS_TYPE',
});

export function parseHostedArguments(argv) {
  assert.ok(Array.isArray(argv), 'hosted arguments must be an array');
  assert.ok(
    argv.length === 0 || (
      argv.length === 1
      && ['--require-reference', '--reference-lifecycles'].includes(argv[0])
    ),
    'hosted accepts only --require-reference or --reference-lifecycles',
  );
  return {
    requireReference: argv.length === 1,
    referenceLifecycles: argv[0] === '--reference-lifecycles',
  };
}

export function officialReferenceFromEnvironment(environment, { requireReference }) {
  const reference = Object.fromEntries(
    Object.entries(REFERENCE_ENVIRONMENT).map(([field, name]) => [field, environment[name]]),
  );
  const configured = Object.values(reference).filter(Boolean).length;
  assert.ok(
    configured === 0 || configured === Object.keys(reference).length,
    'official reference connection and fixture fields must be configured together',
  );
  assert.ok(
    !requireReference || configured === Object.keys(reference).length,
    'release qualification requires the official Anthropic reference service',
  );
  if (configured === 0) return undefined;
  for (const [field, value] of Object.entries(reference)) {
    assert.ok(typeof value === 'string' && value.trim().length > 0, `official reference ${field}`);
  }
  const url = new URL(reference.baseURL);
  assert.ok(['http:', 'https:'].includes(url.protocol), 'official reference baseURL uses HTTP(S)');
  assert.ok(
    ['application', 'passthrough'].includes(reference.userProfileAccessType),
    'official reference userProfileAccessType',
  );
  return reference;
}
