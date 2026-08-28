import assert from 'node:assert/strict';
import test from 'node:test';

import {
  awakenTargetFromEnvironment,
  officialReferenceFromEnvironment,
  parseHostedArguments,
} from '../src/conformance/hosted-configuration.mjs';

const completeReference = Object.freeze({
  ANTHROPIC_MANAGED_REFERENCE_BASE_URL: 'https://reference.invalid',
  ANTHROPIC_MANAGED_REFERENCE_API_KEY: 'reference-key', // awaken-allow: secret
  ANTHROPIC_MANAGED_REFERENCE_TUNNEL_ACCESS_TOKEN: 'reference-token', // awaken-allow: secret
  ANTHROPIC_MANAGED_REFERENCE_AGENT_ID: 'agent-reference',
  ANTHROPIC_MANAGED_REFERENCE_ENVIRONMENT_ID: 'environment-reference',
  ANTHROPIC_MANAGED_REFERENCE_WORKSPACE_ID: 'workspace-reference',
  ANTHROPIC_MANAGED_REFERENCE_USER_PROFILE_ID: 'profile-reference',
  ANTHROPIC_MANAGED_REFERENCE_USER_PROFILE_ACCESS_TYPE: 'application',
});

const completeAwaken = Object.freeze({
  AWAKEN_MANAGED_BASE_URL: 'https://awaken.invalid',
  AWAKEN_MANAGED_API_KEY: 'awaken-key', // awaken-allow: secret
  AWAKEN_MANAGED_TUNNEL_ACCESS_TOKEN: 'awaken-token', // awaken-allow: secret
  AWAKEN_MANAGED_AGENT_ID: 'agent-awaken',
  AWAKEN_MANAGED_ENVIRONMENT_ID: 'environment-awaken',
  AWAKEN_MANAGED_WORKSPACE_ID: 'workspace-awaken',
  AWAKEN_MANAGED_USER_PROFILE_ID: 'profile-awaken',
  AWAKEN_MANAGED_USER_PROFILE_ACCESS_TYPE: 'passthrough',
});

test('hosted invocation has one closed release-mode choice', () => {
  // Decision table: no argument is the developer probe; the two release-owned
  // modes require the reference target and separately own differential and
  // positive-lifecycle evidence. Typos, combinations, and positional values
  // fail before a deployment is touched.
  assert.deepEqual(parseHostedArguments([]), {
    requireReference: false,
    referenceLifecycles: false,
  });
  assert.deepEqual(parseHostedArguments(['--require-reference']), {
    requireReference: true,
    referenceLifecycles: false,
  });
  assert.deepEqual(parseHostedArguments(['--reference-lifecycles']), {
    requireReference: true,
    referenceLifecycles: true,
  });
  for (const invalid of [
    ['--reference'],
    ['--require-reference', '--reference-lifecycles'],
    ['development'],
  ]) {
    assert.throws(() => parseHostedArguments(invalid), /accepts only/u);
  }
});

test('official reference configuration is all-or-nothing and release is fail-closed', () => {
  // Cause/effect graph: R1 all connection and fixture fields -> one reference target; R2
  // none -> optional developer run; R3 every proper partial subset -> reject;
  // R4 release+none -> reject. This exhausts all 2^8 presence combinations
  // and prevents an Awaken-only run from certifying differential compatibility.
  assert.deepEqual(
    officialReferenceFromEnvironment(completeReference, { requireReference: true }),
    {
      baseURL: 'https://reference.invalid',
      apiKey: 'reference-key', // awaken-allow: secret
      tunnelAccessToken: 'reference-token', // awaken-allow: secret
      agent: 'agent-reference',
      environmentId: 'environment-reference',
      workspaceId: 'workspace-reference',
      userProfileId: 'profile-reference',
      userProfileAccessType: 'application',
    },
    'R1',
  );
  assert.equal(
    officialReferenceFromEnvironment({}, { requireReference: false }),
    undefined,
    'R2',
  );
  const names = Object.keys(completeReference);
  for (let mask = 1; mask < (1 << names.length) - 1; mask += 1) {
    const partial = Object.fromEntries(names
      .filter((_, index) => (mask & (1 << index)) !== 0)
      .map((name) => [name, completeReference[name]]));
    assert.throws(
      () => officialReferenceFromEnvironment(partial, { requireReference: false }),
      /configured together/u,
      `R3 mask ${mask}`,
    );
  }
  assert.throws(
    () => officialReferenceFromEnvironment({}, { requireReference: true }),
    /official reference is required/u,
    'R4',
  );
  for (const [name, value, pattern] of [
    ['ANTHROPIC_MANAGED_REFERENCE_BASE_URL', 'file:///tmp/reference', /uses HTTP\(S\)/u],
    ['ANTHROPIC_MANAGED_REFERENCE_AGENT_ID', '   ', /reference agent/u],
    [
      'ANTHROPIC_MANAGED_REFERENCE_USER_PROFILE_ACCESS_TYPE',
      'resold',
      /userProfileAccessType/u,
    ],
  ]) {
    assert.throws(
      () => officialReferenceFromEnvironment(
        { ...completeReference, [name]: value },
        { requireReference: true },
      ),
      pattern,
      `R5 ${name}`,
    );
  }
});

test('Awaken hosted configuration is complete before any release side effect', () => {
  // Admission graph: all eight target coordinates form one immutable value;
  // removing any coordinate, using a blank value, a non-HTTP URL, or an open
  // access type fails before hosted traffic or recovery artifacts can exist.
  assert.deepEqual(awakenTargetFromEnvironment(completeAwaken), {
    baseURL: 'https://awaken.invalid',
    apiKey: 'awaken-key', // awaken-allow: secret
    tunnelAccessToken: 'awaken-token', // awaken-allow: secret
    agent: 'agent-awaken',
    environmentId: 'environment-awaken',
    workspaceId: 'workspace-awaken',
    userProfileId: 'profile-awaken',
    userProfileAccessType: 'passthrough',
  });
  for (const name of Object.keys(completeAwaken)) {
    const incomplete = { ...completeAwaken };
    delete incomplete[name];
    assert.throws(
      () => awakenTargetFromEnvironment(incomplete),
      /configured together/u,
      name,
    );
  }
  assert.throws(() => awakenTargetFromEnvironment({}), /is required/u);
  for (const [name, value, pattern] of [
    ['AWAKEN_MANAGED_BASE_URL', 'file:///tmp/awaken', /uses HTTP\(S\)/u],
    ['AWAKEN_MANAGED_AGENT_ID', ' ', /target agent/u],
    ['AWAKEN_MANAGED_USER_PROFILE_ACCESS_TYPE', 'unknown', /userProfileAccessType/u],
  ]) {
    assert.throws(
      () => awakenTargetFromEnvironment({ ...completeAwaken, [name]: value }),
      pattern,
      name,
    );
  }
});
