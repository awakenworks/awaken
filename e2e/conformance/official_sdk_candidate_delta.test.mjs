import assert from 'node:assert/strict';
import test from 'node:test';

import {
  assertLatestRuntimeOwnsCandidateDelta,
  officialSdkCandidateDeltaCoordinates,
  officialSdkCandidateDeltaFingerprint,
  qualifyOfficialSdkCandidateDelta,
} from './official_sdk_candidate_delta.mjs';

const empty = () => ({ added: [], removed: [], changed: [] });

test('candidate delta ownership fails closed outside exercised change points', () => {
  // Cause/effect graph: C1 no operation is added/removed; C2 only existing Beta
  // Files/Skills request signatures change; C3 only Files/Skills/Webhooks
  // declaration contents change in place; C4 any other delta. Effects: E1 reuse
  // the generic candidate runtime proof; E2 require a new explicit owner.
  // Decision rule D1 C1+C2+C3->E1; D2 C4->E2. This prevents a green smoke of
  // unchanged calls from certifying a newly added or unrelated SDK capability.
  const owned = {
    operations: {
      ...empty(),
      changed: [{ id: 'beta.files.list' }, { id: 'beta.skills.create' }],
    },
    declarations: {
      ...empty(),
      changed: [
        { path: 'beta/files.d.ts' },
        { path: 'beta/skills/versions.d.ts' },
        { path: 'beta/webhooks.d.ts' },
      ],
    },
    runtime: {
      ...empty(),
      changed: [
        { path: 'core/middleware.mjs' },
        { path: 'internal/uploads.mjs' },
        { path: 'lib/sessions/accumulate.mjs' },
        { path: 'resources/beta/files.mjs' },
        { path: 'resources/beta/skills/versions.mjs' },
        { path: 'resources/beta/webhooks.mjs' },
        { path: 'tools/agent-toolset/node.mjs' },
        { path: 'version.mjs' },
      ],
    },
  };
  assert.doesNotThrow(() => assertLatestRuntimeOwnsCandidateDelta(owned), 'D1/E1');

  const rejected = [
    { ...owned, operations: { ...empty(), added: [{ id: 'beta.files.share' }] } },
    { ...owned, operations: { ...empty(), changed: [{ id: 'beta.sessions.create' }] } },
    { ...owned, declarations: { ...empty(), added: [{ path: 'beta/files/new.d.ts' }] } },
    { ...owned, declarations: { ...empty(), changed: [{ path: 'beta/sessions.d.ts' }] } },
    { ...owned, runtime: { ...empty(), added: [{ path: 'lib/new-helper.mjs' }] } },
    { ...owned, runtime: { ...empty(), changed: [{ path: 'lib/tools/SessionToolRunner.mjs' }] } },
  ];
  for (const delta of rejected) {
    assert.throws(() => assertLatestRuntimeOwnsCandidateDelta(delta), undefined, 'D2/E2');
  }
});

test('candidate qualification binds exact content coordinates to executable behavior owners', () => {
  // Fault-injection graph: C1 a reviewed candidate has one exact version pair,
  // full delta hash, and a partition of every changed coordinate; C2 content
  // changes again at the same path; C3 a coordinate is absent or multiply
  // owned. Effects: E1 return the required executable owner set; E2/E3 reject
  // before importing candidate code. This closes the same-path-change loophole:
  // a familiar filename is not evidence that its new implementation was reviewed.
  const delta = {
    currentVersion: '1.0.0',
    candidateVersion: '1.1.0',
    operations: {
      ...empty(),
      changed: [{ id: 'beta.files.list', before: { method: 'get' }, after: { method: 'post' } }],
    },
    declarations: {
      ...empty(),
      changed: [{ path: 'beta/files.d.ts', before: { fingerprint: 'a' }, after: { fingerprint: 'b' } }],
    },
    runtime: {
      ...empty(),
      changed: [{
        path: 'core/middleware.mjs',
        before: { fingerprint: 'a' },
        after: { fingerprint: 'b' },
      }],
    },
  };
  const coordinates = officialSdkCandidateDeltaCoordinates(delta);
  const qualification = {
    baseline_version: '1.0.0',
    candidate_version: '1.1.0',
    delta_fingerprint: officialSdkCandidateDeltaFingerprint(delta),
    evidence_groups: [{ owner: 'candidate.exact-runtime', coordinates }],
  };
  assert.deepEqual(
    qualifyOfficialSdkCandidateDelta(delta, [qualification]),
    { requiredBehaviorOwners: ['candidate.exact-runtime'] },
    'C1/E1',
  );

  const samePathMutation = structuredClone(delta);
  samePathMutation.runtime.changed[0].after.fingerprint = 'c';
  assert.throws(
    () => qualifyOfficialSdkCandidateDelta(samePathMutation, [qualification]),
    /fingerprint is not the reviewed qualification/u,
    'C2/E2',
  );
  assert.throws(
    () => qualifyOfficialSdkCandidateDelta(delta, [{
      ...qualification,
      evidence_groups: [{ owner: 'candidate.exact-runtime', coordinates: coordinates.slice(1) }],
    }]),
    /does not own every exact delta coordinate/u,
    'C3/E3 missing',
  );
  assert.throws(
    () => qualifyOfficialSdkCandidateDelta(delta, [{
      ...qualification,
      evidence_groups: [
        { owner: 'candidate.first', coordinates },
        { owner: 'candidate.second', coordinates: [coordinates[0]] },
      ],
    }]),
    /owns a coordinate twice/u,
    'C3/E3 overlap',
  );
  assert.throws(
    () => qualifyOfficialSdkCandidateDelta(delta, []),
    /requires one exact qualification/u,
    'C1 without reviewed evidence',
  );
  assert.throws(
    () => qualifyOfficialSdkCandidateDelta(delta, [{
      ...qualification,
      evidence_groups: [{ owner: 'candidate.exact-runtime', coordinates: [] }],
    }]),
    /invalid evidence groups/u,
    'C3/E3 malformed group',
  );
});

test('same-version candidate evidence must be byte-equivalent to the current anchor', () => {
  // Decision table: S1 same version and empty delta reuses the trusted anchor;
  // S2 same version with any changed package evidence is a spoof or corruption.
  // S1 succeeds without a redundant qualification; S2 fails before execution.
  const same = {
    currentVersion: '1.0.0',
    candidateVersion: '1.0.0',
    operations: empty(),
    declarations: empty(),
    runtime: empty(),
  };
  assert.deepEqual(
    qualifyOfficialSdkCandidateDelta(same, []),
    { requiredBehaviorOwners: [] },
    'S1',
  );
  const changed = structuredClone(same);
  changed.runtime.changed.push({ path: 'core/middleware.mjs' });
  assert.throws(
    () => qualifyOfficialSdkCandidateDelta(changed, []),
    /same-version SDK package content differs/u,
    'S2',
  );
});
