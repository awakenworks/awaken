import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';

import {
  RELEASE_HOSTED_ARGUMENTS,
  replacementCommandEnvironment,
  runReleaseQualification,
  validateDeploymentReplacementEvidence,
} from '../src/conformance/release-qualification.mjs';
import { withReleaseArtifactDirectory } from '../src/conformance/release.mjs';

test('release owns differential and positive-reference hosted passes exactly once', () => {
  // Ownership matrix: the Awaken pass owns the single all-route differential;
  // the reference pass owns positive lifecycles only. Both are mandatory and
  // ordered, so deleting, duplicating, or combining them changes this closed
  // release application contract rather than silently weakening evidence.
  assert.deepEqual(RELEASE_HOSTED_ARGUMENTS, [
    ['--require-reference'],
    ['--reference-lifecycles'],
  ]);
});

const revision = '0123456789abcdef';
const baseURL = 'https://staging.awaken.invalid/v1/';

function replacementEvidence() {
  return {
    schema_version: 1,
    target_base_url: baseURL,
    before: { revision, instances: ['brain-old-1', 'brain-old-2'] },
    after: { revision, instances: ['brain-new-1', 'brain-new-2'], ready: true },
  };
}

function qualificationSteps({ failAt } = {}) {
  const trace = [];
  const step = (name) => async () => {
    trace.push(name);
    if (failAt === name) throw new Error(`${name} failure`);
  };
  return {
    trace,
    steps: {
      hosted: step('hosted'),
      prepare: step('prepare'),
      replace: step('replace'),
      verify: step('verify'),
      cleanup: step('cleanup'),
    },
  };
}

test('release qualification establishes the only valid evidence order', async () => {
  // Causal chain: official hosted differential -> durable fixture commit ->
  // complete process replacement -> recovery observation -> cleanup. Reordering
  // any node weakens the claim, so the application service owns one fixed trace.
  const { trace, steps } = qualificationSteps();
  await runReleaseQualification(steps);
  assert.deepEqual(trace, ['hosted', 'prepare', 'replace', 'verify', 'cleanup']);
});

test('release qualification compensates every post-prepare failure', async () => {
  // FMECA partitions: failure before durable prepare has no fixture to clean;
  // every failure after prepare invokes cleanup exactly once. Verification is
  // unreachable after replacement failure, and replacement is unreachable
  // after prepare failure.
  const expected = {
    hosted: ['hosted'],
    prepare: ['hosted', 'prepare'],
    replace: ['hosted', 'prepare', 'replace', 'cleanup'],
    verify: ['hosted', 'prepare', 'replace', 'verify', 'cleanup'],
    cleanup: ['hosted', 'prepare', 'replace', 'verify', 'cleanup'],
  };
  for (const failAt of Object.keys(expected)) {
    const { trace, steps } = qualificationSteps({ failAt });
    await assert.rejects(() => runReleaseQualification(steps), new RegExp(`${failAt} failure`, 'u'));
    assert.deepEqual(trace, expected[failAt], failAt);
  }
});

test('release qualification preserves primary and cleanup failures', async () => {
  // Fault composition: a failed recovery plus failed compensation must retain
  // both causes; reporting cleanup alone would hide the compatibility failure,
  // while reporting verification alone would hide leaked staging resources.
  const { trace, steps } = qualificationSteps();
  steps.verify = async () => { trace.push('verify'); throw new Error('verification failure'); };
  steps.cleanup = async () => { trace.push('cleanup'); throw new Error('cleanup failure'); };
  await assert.rejects(
    () => runReleaseQualification(steps),
    (error) => error instanceof AggregateError
      && error.errors.map(({ message }) => message).join(',') === 'verification failure,cleanup failure',
  );
  assert.deepEqual(trace, ['hosted', 'prepare', 'replace', 'verify', 'cleanup']);
});

test('replacement evidence proves exact revision, total turnover, and readiness', () => {
  // Decision table dimensions: schema/shape, exact revision, non-empty unique
  // identities, old/new set intersection, and ready state. Only their single
  // valid conjunction can establish a real restart; each independently
  // falsified cause fails closed.
  const expected = { expectedRevision: revision, expectedBaseURL: baseURL };
  assert.deepEqual(validateDeploymentReplacementEvidence(replacementEvidence(), expected), replacementEvidence());
  const mutations = [
    [(value) => { value.schema_version = 2; }, /schema/u],
    [(value) => { value.extra = true; }, /fields/u],
    [(value) => { value.target_base_url = 'https://other.invalid'; }, /target base URL/u],
    [(value) => { value.target_base_url = 'https://user:secret@staging.awaken.invalid/v1/'; }, /embedded username/u],
    [(value) => { value.before.revision = 'other'; }, /before replacement revision/u],
    [(value) => { value.after.revision = 'other'; }, /after replacement revision/u],
    [(value) => { value.before.instances = []; }, /before replacement instances/u],
    [(value) => { value.before.instances = ['brain-old-1', 'brain-old-1']; }, /unique/u],
    [(value) => { value.after.instances = []; }, /after replacement instances/u],
    [(value) => { value.after.instances = ['brain-new-1', 'brain-new-1']; }, /unique/u],
    [(value) => { value.after.instances = ['brain-new-1', 2]; }, /instance identities/u],
    [(value) => { value.after.extra = true; }, /after replacement fields/u],
    [(value) => { value.after.instances[0] = 'brain-old-1'; }, /every serving process was replaced/u],
    [(value) => { value.after.ready = false; }, /is ready/u],
  ];
  for (const [mutate, pattern] of mutations) {
    const invalid = replacementEvidence();
    mutate(invalid);
    assert.throws(() => validateDeploymentReplacementEvidence(invalid, expected), pattern);
  }
});

test('package exposes one release-grade command instead of parallel partial gates', async () => {
  // Entry-point ownership: hosted and recovery phases remain diagnostic tools;
  // exactly one package command may claim release qualification, and it points
  // at the application orchestrator rather than a subset phase.
  const manifest = JSON.parse(await fs.promises.readFile(
    new URL('../package.json', import.meta.url),
    'utf8',
  ));
  assert.deepEqual(
    Object.entries(manifest.scripts).filter(([name]) => name.includes('release')),
    [['conformance:release', 'node src/conformance/release.mjs']],
  );
});

test('deployment replacement hook cannot receive Managed service credentials', () => {
  // Least-authority boundary: rollout infrastructure needs deployment identity,
  // never customer/API/Tunnel credentials. The evidence path is injected while
  // unrelated orchestration variables survive unchanged.
  const environment = {
    AWAKEN_MANAGED_API_KEY: 'actual-key', // awaken-allow: secret
    AWAKEN_MANAGED_TUNNEL_ACCESS_TOKEN: 'actual-token', // awaken-allow: secret
    ANTHROPIC_MANAGED_REFERENCE_API_KEY: 'reference-key', // awaken-allow: secret
    ANTHROPIC_MANAGED_REFERENCE_TUNNEL_ACCESS_TOKEN: 'reference-token', // awaken-allow: secret
    KUBECONFIG: '/qualification/kubeconfig',
  };
  assert.deepEqual(replacementCommandEnvironment(environment, '/tmp/evidence.json'), {
    AWAKEN_MANAGED_REPLACEMENT_EVIDENCE_FILE: '/tmp/evidence.json',
    KUBECONFIG: '/qualification/kubeconfig',
  });
  assert.deepEqual(environment, {
    AWAKEN_MANAGED_API_KEY: 'actual-key', // awaken-allow: secret
    AWAKEN_MANAGED_TUNNEL_ACCESS_TOKEN: 'actual-token', // awaken-allow: secret
    ANTHROPIC_MANAGED_REFERENCE_API_KEY: 'reference-key', // awaken-allow: secret
    ANTHROPIC_MANAGED_REFERENCE_TUNNEL_ACCESS_TOKEN: 'reference-token', // awaken-allow: secret
    KUBECONFIG: '/qualification/kubeconfig',
  }, 'caller environment is immutable');
});

test('release artifacts are removed only after successful qualification', async () => {
  // Commit/compensation table: a successful qualification has no remaining
  // recovery duty and deletes its private directory; any failure retains the
  // exact state/evidence files and reports their path so cleanup can be retried.
  let successfulDirectory;
  await withReleaseArtifactDirectory(async (directory) => {
    successfulDirectory = directory;
    fs.writeFileSync(path.join(directory, 'evidence.json'), '{}');
  });
  assert.equal(fs.existsSync(successfulDirectory), false, 'success removes evidence');

  let failedDirectory;
  await assert.rejects(
    () => withReleaseArtifactDirectory(async (directory) => {
      failedDirectory = directory;
      fs.writeFileSync(path.join(directory, 'recovery.json'), '{}');
      throw new Error('injected qualification failure');
    }),
    (error) => error.message.includes('injected qualification failure')
      && error.message.includes(failedDirectory),
  );
  try {
    assert.equal(fs.existsSync(failedDirectory), true, 'failure preserves evidence');
    assert.equal(fs.existsSync(path.join(failedDirectory, 'recovery.json')), true);
  } finally {
    fs.rmSync(failedDirectory, { recursive: true, force: true });
  }
});
