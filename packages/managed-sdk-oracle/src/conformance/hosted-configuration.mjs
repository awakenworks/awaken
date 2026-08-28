import assert from 'node:assert/strict';

const AWAKEN_ENVIRONMENT = Object.freeze({
  baseURL: 'AWAKEN_MANAGED_BASE_URL',
  apiKey: 'AWAKEN_MANAGED_API_KEY', // awaken-allow: secret
  tunnelAccessToken: 'AWAKEN_MANAGED_TUNNEL_ACCESS_TOKEN', // awaken-allow: secret
  agent: 'AWAKEN_MANAGED_AGENT_ID',
  environmentId: 'AWAKEN_MANAGED_ENVIRONMENT_ID',
  workspaceId: 'AWAKEN_MANAGED_WORKSPACE_ID',
  userProfileId: 'AWAKEN_MANAGED_USER_PROFILE_ID',
  userProfileAccessType: 'AWAKEN_MANAGED_USER_PROFILE_ACCESS_TYPE',
});

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

function hostedTargetFromEnvironment(environment, names, { label, required }) {
  const target = Object.fromEntries(
    Object.entries(names).map(([field, name]) => [field, environment[name]]),
  );
  const configured = Object.values(target).filter((value) => value !== undefined).length;
  assert.ok(
    configured === 0 || configured === Object.keys(target).length,
    `${label} connection and fixture fields must be configured together`,
  );
  assert.ok(!required || configured === Object.keys(target).length, `${label} is required`);
  if (configured === 0) return undefined;
  for (const [field, value] of Object.entries(target)) {
    assert.ok(typeof value === 'string' && value.trim().length > 0, `${label} ${field}`);
  }
  const url = new URL(target.baseURL);
  assert.ok(['http:', 'https:'].includes(url.protocol), `${label} baseURL uses HTTP(S)`);
  assert.ok(
    ['application', 'passthrough'].includes(target.userProfileAccessType),
    `${label} userProfileAccessType`,
  );
  return Object.freeze(target);
}

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
  return hostedTargetFromEnvironment(environment, REFERENCE_ENVIRONMENT, {
    label: 'official reference',
    required: requireReference,
  });
}

export function awakenTargetFromEnvironment(environment) {
  return hostedTargetFromEnvironment(environment, AWAKEN_ENVIRONMENT, {
    label: 'Awaken hosted target',
    required: true,
  });
}
