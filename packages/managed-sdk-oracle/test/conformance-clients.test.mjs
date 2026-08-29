import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  currentAndCandidateClients,
  installedPackageVersion,
  loadConformanceClients,
  loadQualifiedClients,
  qualifiedClient,
  readSdkMatrix,
  validateSdkMatrix,
} from '../src/conformance/clients.mjs';
import { exerciseUserProfileChangePoint } from '../src/conformance/user-profile-change-point.mjs';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../..');

test('the Open anchor matrix is the only executable SDK version authority', async () => {
  // Cause/effect graph M1: exact package aliases plus unique semantic roles
  // produce one executable client per reviewed anchor. Duplicate ids, modules,
  // roles, or an unreviewed role fail before any compatibility claim. The
  // 0.121 Beta-resource projection remains an anchor after 0.122 promotion, so
  // promotion cannot silently narrow the supported-version evidence set.
  const matrix = validateSdkMatrix(readSdkMatrix());
  const clients = await loadQualifiedClients(matrix);
  assert.equal(clients.length, matrix.length, 'M1');
  for (const client of clients) {
    assert.equal(client.version, installedPackageVersion(client.module), 'M1');
    assert.equal(typeof client.Client, 'function', 'M1');
  }
  assert.ok(
    qualifiedClient(clients, 'oldest_supported').version.localeCompare(
      qualifiedClient(clients, 'current_oracle').version,
      undefined,
      { numeric: true },
    ) < 0,
    'M1',
  );

  assert.throws(
    () => validateSdkMatrix([...matrix, { ...matrix[0], id: 'duplicate' }]),
    /duplicate SDK module/u,
    'M1',
  );
  assert.throws(
    () => validateSdkMatrix(matrix.slice(1)),
    /missing oldest_supported/u,
    'M1',
  );
});

test('every multi-version Managed E2E consumes the canonical anchor matrix', () => {
  // Cause/effect graph M2: a second version alias or a locally curated client
  // array can silently leave a released SDK unqualified. Therefore the E2E
  // package owns no versioned Anthropic alias, and every cross-version suite
  // imports the canonical loader. Adding an anchor changes all these matrices
  // without another dependency or hand-maintained version list.
  const e2ePackage = JSON.parse(fs.readFileSync(path.join(repoRoot, 'e2e/package.json'), 'utf8'));
  const dependencies = {
    ...e2ePackage.dependencies,
    ...e2ePackage.devDependencies,
  };
  assert.deepEqual(
    Object.keys(dependencies).filter((name) => /^@anthropic-ai\/sdk-/u.test(name)),
    [],
    'M2: versioned SDK aliases belong only to managed-sdk-oracle',
  );

  const matrixSuites = [
    'e2e/conformance/managed_sdk_memory_depth_e2e.mjs',
    'e2e/conformance/managed_sdk_resource_handoff_e2e.mjs',
    'e2e/conformance/managed_sdk_runtime_matrix_e2e.mjs',
    'e2e/conformance/managed_sdk_version_matrix_e2e.mjs',
    'e2e/managed_webhooks_official_sdk_e2e.mjs',
  ];
  for (const relativePath of matrixSuites) {
    const source = fs.readFileSync(path.join(repoRoot, relativePath), 'utf8');
    assert.match(source, /loadConformanceClients/u, `M2: ${relativePath}`);
    assert.doesNotMatch(source, /@anthropic-ai\/sdk-\d/u, `M2: ${relativePath}`);
  }
});

test('a reviewed candidate joins every shared conformance suite without becoming an anchor', async () => {
  // Candidate admission graph: C1=the stable reviewed-role matrix is valid;
  // C2=the caller supplies the sole reviewed package alias. Effects: E1=the
  // exact candidate is appended once with an explicit non-anchor role; E2=the
  // stable matrix remains unchanged; E3=an arbitrary import specifier fails
  // before module evaluation. This lets one implementation drive current and
  // candidate tests without silently promoting registry-latest code.
  const matrix = readSdkMatrix();
  const candidateVersion = installedPackageVersion('@anthropic-ai/sdk-candidate');
  const clients = await loadConformanceClients(
    matrix,
    '@anthropic-ai/sdk-candidate',
    candidateVersion,
  );
  assert.equal(clients.length, matrix.length + 1, 'E1');
  assert.equal(clients.at(-1).role, 'candidate', 'E1');
  assert.equal(clients.at(-1).version, candidateVersion, 'E1');
  assert.deepEqual(
    currentAndCandidateClients(clients).map(({ role }) => role),
    ['current_oracle', 'candidate'],
    'E1: release-depth suites execute current and candidate',
  );
  assert.deepEqual(readSdkMatrix(), matrix, 'E2');
  await assert.rejects(
    loadConformanceClients(matrix, '@anthropic-ai/sdk', candidateVersion),
    /reviewed exact package alias/u,
    'E3',
  );
  await assert.rejects(
    loadConformanceClients(matrix, '@anthropic-ai/sdk-candidate', '9.9.9'),
    /must match its reviewed version/u,
    'E3',
  );
  assert.throws(
    () => currentAndCandidateClients([
      ...clients,
      { ...clients.at(-1), id: 'candidate-duplicate' },
    ]),
    /at most one reviewed candidate/u,
    'E3: ambiguous candidate evidence fails closed',
  );

  for (const relativePath of [
    'packages/managed-sdk-oracle/src/conformance/hosted.mjs',
    'packages/managed-sdk-oracle/src/conformance/recovery.mjs',
  ]) {
    const source = fs.readFileSync(path.join(repoRoot, relativePath), 'utf8');
    assert.match(source, /loadConformanceClients/u, `${relativePath} admits the exact candidate`);
    assert.doesNotMatch(source, /loadQualifiedClients/u, `${relativePath} cannot bypass admission`);
  }

  const scripts = JSON.parse(
    fs.readFileSync(path.join(repoRoot, 'e2e/package.json'), 'utf8'),
  ).scripts;
  const candidateMatrix = scripts['test:managed-sdk-candidate-matrix'];
  for (const owner of [
    'sdk-transport-resilience.test.mjs',
    'managed_sdk_version_matrix_e2e.mjs',
    'managed_sdk_runtime_matrix_e2e.mjs',
    'managed_sdk_resource_handoff_e2e.mjs',
    'managed_sdk_memory_depth_e2e.mjs',
    'managed_webhooks_official_sdk_e2e.mjs',
  ]) {
    assert.equal(candidateMatrix.split(owner).length, 2, `${owner} executes exactly once`);
  }
});

function profileClient(profile) {
  return {
    beta: { userProfiles: { retrieve: async () => structuredClone(profile) } },
  };
}

function profiles(accessType) {
  const relationship = accessType === 'application' ? 'external' : 'resold';
  const stable = {
    id: 'uprof_fixture',
    external_id: 'subject-1',
    name: 'Fixture',
    metadata: { tier: 'test' },
  };
  return {
    legacy: { ...stable, relationship },
    current: { ...stable, relationship, access_type: accessType },
  };
}

for (const accessType of ['application', 'passthrough']) {
  test(`User Profiles ${accessType} change point preserves one semantic aggregate`, async () => {
    // Cause/effect graph U1/U2: each legacy relationship and current
    // access_type pair must project one identity and stable metadata.
    const fixture = profiles(accessType);
    await exerciseUserProfileChangePoint({
      profileID: fixture.current.id,
      expectedAccessType: accessType,
      legacyClient: profileClient(fixture.legacy),
      currentClient: profileClient(fixture.current),
    });
  });
}

test('User Profiles change point rejects semantic, identity, and metadata drift', async () => {
  // Negative decision table U3: decoding both SDK responses is insufficient;
  // wrong mapping, identity drift, metadata drift, and an open access enum all
  // fail closed.
  const fixture = profiles('application');
  const cases = [
    [{ ...fixture.legacy, relationship: 'resold' }, fixture.current, /legacy relationship/u],
    [fixture.legacy, { ...fixture.current, id: 'uprof_other' }, /current anchor/u],
    [fixture.legacy, { ...fixture.current, metadata: { tier: 'drift' } }, /metadata remains stable/u],
  ];
  for (const [legacy, current, pattern] of cases) {
    await assert.rejects(
      exerciseUserProfileChangePoint({
        profileID: fixture.legacy.id,
        expectedAccessType: 'application',
        legacyClient: profileClient(legacy),
        currentClient: profileClient(current),
      }),
      pattern,
      'U3',
    );
  }
  await assert.rejects(
    exerciseUserProfileChangePoint({
      profileID: fixture.legacy.id,
      expectedAccessType: 'unknown',
      legacyClient: profileClient(fixture.legacy),
      currentClient: profileClient(fixture.current),
    }),
    /expectedAccessType/u,
    'U3',
  );
});
