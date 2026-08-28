import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  installedPackageVersion,
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
  // roles, or an unreviewed fourth role fail before any compatibility claim.
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
    assert.match(source, /loadQualifiedClients/u, `M2: ${relativePath}`);
    assert.doesNotMatch(source, /@anthropic-ai\/sdk-\d/u, `M2: ${relativePath}`);
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
