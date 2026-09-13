import assert from 'node:assert/strict';
import test from 'node:test';

import {
  assertLatestRuntimeOwnsCandidateDelta,
  assertRetainedOfficialSdkBehaviorOwners,
  officialSdkCandidateDeltaCoordinates,
  officialSdkCandidateDeltaFingerprint,
  qualifyOfficialSdkCandidateDelta,
  assertDeclarationWitnesses,
  declarationCoordinatePath,
} from './official_sdk_candidate_delta.mjs';

const empty = () => ({ added: [], removed: [], changed: [] });

test('candidate delta ownership fails closed outside exercised change points', () => {
  // Cause/effect graph: C1 no operation is added/removed; C2 only existing
  // operations already exercised by a candidate owner change; C3 only scoped
  // declarations and runtime modules with an executable owner change in place,
  // except the one reviewed Node facade addition; C4 any other delta. Effects:
  // E1 reuse the exact candidate runtime proof; E2 require a new explicit owner.
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
        { path: 'beta/agents/agents.d.ts' },
        { path: 'beta/dreams.d.ts' },
        { path: 'beta/user-profiles.d.ts' },
        { path: 'beta/skills/versions.d.ts' },
        { path: 'beta/webhooks.d.ts' },
        { path: 'managed/lib/sessions/accumulate.d.ts' },
        { path: 'managed/lib/environments/worker.d.ts' },
        { path: 'managed/tools/agent-toolset/node.d.ts' },
        { path: 'managed/tools/agent-toolset/skills.d.ts' },
      ],
    },
    runtime: {
      ...empty(),
      added: [{ path: 'internal/node.mjs' }],
      changed: [
        { path: 'client.mjs' },
        { path: 'core/streaming.mjs' },
        { path: 'core/credentials.mjs' },
        { path: 'core/middleware.mjs' },
        { path: 'internal/file-store.mjs' },
        { path: 'internal/uploads.mjs' },
        { path: 'lib/credentials/identity-token.mjs' },
        { path: 'lib/middleware.mjs' },
        { path: 'lib/sessions/accumulate.mjs' },
        { path: 'lib/environments/worker.mjs' },
        { path: 'core/pagination.mjs' },
        { path: 'internal/utils/uuid.mjs' },
        { path: 'resources/skills/index.mjs' },
        { path: 'resources/beta/files.mjs' },
        { path: 'resources/beta/sessions/threads/events.mjs' },
        { path: 'resources/beta/skills/versions.mjs' },
        { path: 'resources/models.mjs' },
        { path: 'resources/beta/webhooks.mjs' },
        { path: 'tools/agent-toolset/fs-util.mjs' },
        { path: 'tools/agent-toolset/memories.mjs' },
        { path: 'tools/agent-toolset/node.mjs' },
        { path: 'version.mjs' },
      ],
    },
    exports: {
      ...empty(),
      removed: [{ id: 'tools/agent-toolset/node.mjs#resolveSkillVersion' }],
    },
  };
  assert.doesNotThrow(() => assertLatestRuntimeOwnsCandidateDelta(owned), 'D1/E1');

  const rejected = [
    { ...owned, operations: { ...empty(), added: [{ id: 'beta.files.share' }] } },
    { ...owned, operations: { ...empty(), changed: [{ id: 'beta.sessions.create' }] } },
    { ...owned, declarations: { ...empty(), added: [{ path: 'beta/files/new.d.ts' }] } },
    { ...owned, declarations: { ...empty(), changed: [{ path: 'organization/admin.d.ts' }] } },
    { ...owned, runtime: { ...empty(), added: [{ path: 'lib/new-helper.mjs' }] } },
    { ...owned, runtime: { ...empty(), changed: [{ path: 'lib/tools/SessionToolRunner.mjs' }] } },
    { ...owned, runtime: { ...empty(), changed: [{ path: 'internal/utils/sleep.mjs' }] } },
    { ...owned, runtime: { ...empty(), changed: [{ path: 'lib/environments/index.mjs' }] } },
    { ...owned, declarations: { ...empty(), changed: [{ path: 'managed/lib/environments/index.d.ts' }] } },
    { ...owned, exports: { ...empty(), added: [{ id: 'tools/agent-toolset/node.mjs#newTool' }] } },
    { ...owned, exports: { ...empty(), removed: [{ id: 'tools/agent-toolset/node.mjs#setupSkills' }] } },
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
    exports: empty(),
  };
  const coordinates = officialSdkCandidateDeltaCoordinates(delta);
  const witnesses = { 'declarations:changed:beta/files.d.ts': 'expires_at\\?: string' };
  const qualification = {
    baseline_version: '1.0.0',
    candidate_version: '1.1.0',
    delta_fingerprint: officialSdkCandidateDeltaFingerprint(delta),
    evidence_groups: [{ owner: 'candidate.exact-runtime', coordinates, witnesses }],
  };
  const qualified = qualifyOfficialSdkCandidateDelta(delta, [qualification]);
  assert.deepEqual(qualified.requiredBehaviorOwners, ['candidate.exact-runtime'], 'C1/E1');
  assert.deepEqual(
    qualified.witnesses.map(({ owner, coordinate, regexp }) => [owner, coordinate, regexp.source]),
    [['candidate.exact-runtime', 'declarations:changed:beta/files.d.ts', 'expires_at\\?: string']],
    'C1/E1 a changed declaration is owned by what was read in it',
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

test('a changed declaration is owned only by a witness that matches what changed', () => {
  // Fault-injection graph: C1 an owner claims a changed `.d.ts` coordinate;
  // C2 it names no witness; C3 it names a witness the candidate file does not
  // contain; C4 it witnesses a coordinate it does not own. Effects: E1 a
  // witnessed claim resolves to the file and matches; E2/E3/E4 the claim is
  // rejected before the owner's proof can be credited. This is the gap that
  // let 0.125's `evaluation` field arrive unreviewed: the events declaration
  // was claimed by an owner whose proof read a different file.
  const delta = {
    currentVersion: '1.0.0',
    candidateVersion: '1.1.0',
    operations: empty(),
    declarations: {
      ...empty(),
      changed: [{
        path: 'beta/sessions/events.d.ts',
        before: { fingerprint: 'a' },
        after: { fingerprint: 'b' },
      }],
    },
    runtime: empty(),
    exports: empty(),
  };
  const coordinates = officialSdkCandidateDeltaCoordinates(delta);
  const base = {
    baseline_version: '1.0.0',
    candidate_version: '1.1.0',
    delta_fingerprint: officialSdkCandidateDeltaFingerprint(delta),
  };
  const witnessed = {
    ...base,
    evidence_groups: [{
      owner: 'candidate.events',
      coordinates,
      witnesses: { [coordinates[0]]: 'evaluation\\?: BetaManagedAgentsAgentToolEvaluation' },
    }],
  };
  const { witnesses } = qualifyOfficialSdkCandidateDelta(delta, [witnessed]);
  const files = {
    'resources/beta/sessions/events.d.ts': 'export interface X { evaluation?: BetaManagedAgentsAgentToolEvaluation; }',
  };
  assert.equal(declarationCoordinatePath(coordinates[0]), 'resources/beta/sessions/events.d.ts', 'C1/E1');
  assertDeclarationWitnesses(witnesses, (relative) => files[relative]);
  assert.throws(
    () => assertDeclarationWitnesses(witnesses, () => 'export interface X { name: string }'),
    /does not match the candidate declaration/u,
    'C3/E3',
  );
  assert.throws(
    () => qualifyOfficialSdkCandidateDelta(delta, [{
      ...base,
      evidence_groups: [{ owner: 'candidate.events', coordinates }],
    }]),
    /without a witness pattern/u,
    'C2/E2',
  );
  assert.throws(
    () => qualifyOfficialSdkCandidateDelta(delta, [{
      ...base,
      evidence_groups: [{
        owner: 'candidate.events',
        coordinates,
        witnesses: {
          [coordinates[0]]: 'evaluation',
          'declarations:changed:beta/agents/agents.d.ts': 'AutoPolicy',
        },
      }],
    }]),
    /witnesses .* it does not own/u,
    'C4/E4',
  );
  assert.equal(declarationCoordinatePath('declarations:changed:managed/lib/environments/worker.d.ts'), 'lib/environments/worker.d.ts');
  assert.equal(declarationCoordinatePath('declarations:changed:ga/files.d.ts'), 'resources/files.d.ts');
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
    exports: empty(),
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

test('promotion retains prior behavior obligations without importing future requirements', () => {
  // Cause/effect graph: C1 A->B->C and A->C reviewed upgrades; C2 C is promoted so its
  // immediate candidate delta is empty; C3 an owner occurs in both upgrades;
  // C4 a future/unrelated edge exists; C5 an earlier owner did not complete.
  // Rules P1 C1+C2+C3 -> require the union once; P2 C4 -> no added requirement;
  // P3 C5 -> reject even with an empty current delta. Tests use literal expected
  // owners independently of the traversal, including each single missing owner.
  const entry = (baseline, candidate, owners) => ({
    baseline_version: baseline, candidate_version: candidate,
    evidence_groups: owners.map(owner => ({ owner })),
  });
  const catalog = [
    entry('A', 'B', ['stream', 'transport']),
    entry('B', 'C', ['workspace', 'transport']),
    entry('A', 'C', ['transport']),
    entry('C', 'D', ['future']),
    entry('X', 'Y', ['unrelated']),
  ];
  const expected = ['stream', 'transport', 'workspace'];
  const sameVersion = {
    currentVersion: 'C', candidateVersion: 'C',
    operations: empty(), declarations: empty(), runtime: empty(), exports: empty(),
  };
  assert.deepEqual(qualifyOfficialSdkCandidateDelta(sameVersion, catalog).requiredBehaviorOwners, []);
  assert.deepEqual(assertRetainedOfficialSdkBehaviorOwners('C', catalog, new Set(expected)), expected, 'P1/P2');
  for (const missing of expected) {
    assert.throws(
      () => assertRetainedOfficialSdkBehaviorOwners('C', catalog, new Set(expected.filter(owner => owner !== missing))),
      { message: `retained SDK behavior owner did not complete: ${missing}` },
      `P3 ${missing}`,
    );
  }
  assert.deepEqual(assertRetainedOfficialSdkBehaviorOwners('B', catalog, new Set(['stream', 'transport'])), ['stream', 'transport']);
});
