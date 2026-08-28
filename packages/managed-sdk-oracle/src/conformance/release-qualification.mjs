import assert from 'node:assert/strict';
import fs from 'node:fs';

const SECRET_ENVIRONMENT_NAMES = Object.freeze([
  'AWAKEN_MANAGED_API_KEY',
  'AWAKEN_MANAGED_TUNNEL_ACCESS_TOKEN',
  'ANTHROPIC_MANAGED_REFERENCE_API_KEY',
  'ANTHROPIC_MANAGED_REFERENCE_TUNNEL_ACCESS_TOKEN',
]);

export const RELEASE_HOSTED_ARGUMENTS = Object.freeze([
  Object.freeze(['--require-reference']),
  Object.freeze(['--reference-lifecycles']),
]);

function exactKeys(value, expected, label) {
  assert.deepEqual(Object.keys(value).sort(), [...expected].sort(), `${label} fields`);
}

function deploymentSnapshot(value, label, expectedRevision, readyRequired) {
  assert.ok(value && typeof value === 'object' && !Array.isArray(value), `${label} is an object`);
  exactKeys(value, readyRequired ? ['instances', 'ready', 'revision'] : ['instances', 'revision'], label);
  assert.equal(value.revision, expectedRevision, `${label} revision`);
  assert.ok(Array.isArray(value.instances) && value.instances.length > 0, `${label} instances`);
  assert.ok(
    value.instances.every((instance) => typeof instance === 'string' && instance.length > 0),
    `${label} instance identities`,
  );
  assert.equal(new Set(value.instances).size, value.instances.length, `${label} instances are unique`);
  if (readyRequired) assert.equal(value.ready, true, `${label} is ready`);
  return new Set(value.instances);
}

function canonicalBaseURL(value, label) {
  const url = new URL(value);
  assert.ok(['http:', 'https:'].includes(url.protocol), `${label} uses HTTP(S)`);
  assert.equal(url.username, '', `${label} contains no embedded username`);
  assert.equal(url.password, '', `${label} contains no embedded password`);
  assert.equal(url.search, '', `${label} contains no query`);
  assert.equal(url.hash, '', `${label} contains no fragment`);
  return url.href;
}

export function validateDeploymentReplacementEvidence(evidence, { expectedRevision, expectedBaseURL }) {
  assert.match(expectedRevision ?? '', /^[0-9A-Za-z][0-9A-Za-z._-]*$/u, 'exact deployed revision');
  assert.ok(evidence && typeof evidence === 'object' && !Array.isArray(evidence), 'replacement evidence is an object');
  exactKeys(
    evidence,
    ['after', 'before', 'schema_version', 'target_base_url'],
    'replacement evidence',
  );
  assert.equal(evidence.schema_version, 1, 'replacement evidence schema');
  assert.equal(
    canonicalBaseURL(evidence.target_base_url, 'replacement target'),
    canonicalBaseURL(expectedBaseURL, 'expected target'),
    'replacement target base URL',
  );
  const before = deploymentSnapshot(evidence.before, 'before replacement', expectedRevision, false);
  const after = deploymentSnapshot(evidence.after, 'after replacement', expectedRevision, true);
  assert.ok(
    [...before].every((instance) => !after.has(instance)),
    'every serving process was replaced',
  );
  return evidence;
}

export function replacementCommandEnvironment(environment, evidenceFile) {
  assert.ok(typeof evidenceFile === 'string' && evidenceFile.length > 0, 'replacement evidence file');
  const childEnvironment = { ...environment, AWAKEN_MANAGED_REPLACEMENT_EVIDENCE_FILE: evidenceFile };
  for (const name of SECRET_ENVIRONMENT_NAMES) delete childEnvironment[name];
  return childEnvironment;
}

export function readDeploymentReplacementEvidence(evidenceFile, expectedDeployment) {
  const evidence = JSON.parse(fs.readFileSync(evidenceFile, 'utf8'));
  return validateDeploymentReplacementEvidence(evidence, expectedDeployment);
}

export async function runReleaseQualification({
  hosted,
  prepare,
  replace,
  verify,
  cleanup,
}) {
  await hosted();
  await prepare();
  let primaryFailure;
  try {
    await replace();
    await verify();
  } catch (error) {
    primaryFailure = error;
  }

  let cleanupFailure;
  try {
    await cleanup();
  } catch (error) {
    cleanupFailure = error;
  }

  if (primaryFailure && cleanupFailure) {
    throw new AggregateError(
      [primaryFailure, cleanupFailure],
      'Managed release qualification and cleanup both failed',
    );
  }
  if (primaryFailure) throw primaryFailure;
  if (cleanupFailure) throw cleanupFailure;
}
