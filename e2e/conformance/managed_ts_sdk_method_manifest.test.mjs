import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';
import {
  loadQualifiedClients,
  qualifiedClient,
} from '../../packages/managed-sdk-oracle/src/conformance/clients.mjs';
import { extractOperationsFromPackageRoot } from '../../packages/managed-sdk-oracle/src/extract-operations.mjs';
import { resolveSdkPackage } from '../../packages/managed-sdk-oracle/src/package-source.mjs';
import {
  MANAGED_TS_METHOD_MANIFEST,
  managedTsMethodManifestForOperations,
} from './managed_ts_sdk_method_manifest.mjs';

const E2E = resolve(import.meta.dirname, '..');
const Anthropic = qualifiedClient(await loadQualifiedClients(), 'current_oracle').Client;
const operationCoverage = JSON.parse(readFileSync(resolve(
  E2E,
  '../contracts/anthropic-managed/operation-coverage.generated.json',
), 'utf8'));
const scope = JSON.parse(readFileSync(resolve(
  E2E,
  '../packages/managed-sdk-oracle/config/scope.json',
), 'utf8'));

function publicMethods(resource, prefix = '', depth = 0, found = []) {
  if (!resource || depth > 4) return found;
  for (const name of Object.getOwnPropertyNames(Object.getPrototypeOf(resource) ?? {})) {
    if (name !== 'constructor' && typeof resource[name] === 'function') found.push(`${prefix}${name}`);
  }
  for (const name of Object.keys(resource)) {
    if (name !== '_client' && resource[name] && typeof resource[name] === 'object') {
      publicMethods(resource[name], `${prefix}${name}.`, depth + 1, found);
    }
  }
  return found;
}

function officialManagedMethods(client) {
  const betaRoots = new Set(MANAGED_TS_METHOD_MANIFEST
    .filter(({ sdkRoot }) => sdkRoot === 'beta')
    .map(({ relativeMethod }) => relativeMethod.split('.')[0]));
  return [
    ...publicMethods(client.beta)
      .filter((method) => betaRoots.has(method.split('.')[0]))
      .map((method) => `beta.${method}`),
    ...publicMethods(client.models, 'models.'),
    ...publicMethods(client.files, 'files.'),
    ...publicMethods(client.skills, 'skills.'),
  ].sort();
}

function assertExactMethodInventory(manifest, actual) {
  const expected = manifest.map((entry) => entry.sdkMethod).sort();
  assert.equal(new Set(expected).size, expected.length, 'each SDK method appears exactly once');
  assert.deepEqual(expected, actual, 'new/removed SDK methods require an explicit manifest decision');
}

test('real-process ownership derives every HTTP method from the canonical operation ledger', () => {
  // Cause/effect graph: C1=current official SDK operations are generated once
  // into the canonical ledger; C2=the E2E ownership projection adds only four
  // non-HTTP generated helpers. Effects: E1=every HTTP id and route is identical
  // to C1, E2=no stale hand-copied operation can survive, E3=documented
  // SDK-absent routes cannot be presented as SDK calls. Decision table:
  // C1+C2 -> E1+E2+E3; a missing, duplicate, renamed, or extra HTTP row fails
  // exact map equality before source-level invocation checks run.
  const expected = new Map(operationCoverage.operations
    .filter(({ id }) => !id.startsWith('documented.'))
    .map(({ id, path, method, betas, transport_query: transportQuery }) => [id, {
      route: path,
      method,
      betas,
      ...(transportQuery ? { transportQuery } : {}),
    }]));
  const helpers = MANAGED_TS_METHOD_MANIFEST
    .filter(({ route }) => route.startsWith('generated-') || route === 'offline-standard-webhooks');
  const actual = new Map(MANAGED_TS_METHOD_MANIFEST
    .filter(({ sdkMethod }) => expected.has(sdkMethod))
    .map(({ sdkMethod, route, method, betas, transportQuery }) => [sdkMethod, {
      route,
      method,
      betas,
      ...(transportQuery ? { transportQuery } : {}),
    }]));
  assert.deepEqual(actual, expected, 'C1/E1/E2/E3');
  assert.deepEqual(
    helpers.map(({ sdkMethod }) => sdkMethod).sort(),
    [
      'beta.environments.work.poller',
      'beta.environments.work.worker',
      'beta.sessions.events.toolRunner',
      'beta.webhooks.unwrap',
    ],
    'C2/E2',
  );
  assert.equal(MANAGED_TS_METHOD_MANIFEST.length, expected.size + helpers.length);
});

test('every official TypeScript Managed SDK method has one executable owner', () => {
  // Cause/effect graph: C1=the Beta Managed methods exactly match the manifest;
  // C2=the GA Models/Files/Skills methods exactly match the same manifest;
  // C3=each root-qualified method appears once with a route; C4=its named E2E
  // owner calls that exact Beta or GA entrypoint directly (or one explicit
  // canonical helper); C5=the owner is reachable from the deterministic graph.
  // Effect E1=the SDK surface has one executable owner. Decision rule R1:
  // C1 && C2 && C3 && C4 && C5 => E1; any false condition fails with that missing
  // ownership edge instead of accepting a parallel or unexecuted evidence table.
  // Constraints/invariant: Beta and GA share one inventory but retain distinct
  // call identities; package orchestration, not a second manifest, proves reachability.
  const client = new Anthropic({ apiKey: 'surface-inventory' }); // awaken-allow: secret
  const actual = officialManagedMethods(client);
  assertExactMethodInventory(MANAGED_TS_METHOD_MANIFEST, actual);

  const orchestration = [
    readFileSync(resolve(E2E, 'package.json'), 'utf8'),
    readFileSync(resolve(E2E, 'stage_change_coverage_e2e.ts'), 'utf8'),
  ].join('\n');

  for (const evidence of MANAGED_TS_METHOD_MANIFEST) {
    assert.ok(evidence.route, `${evidence.sdkMethod}: route`);
    const source = readFileSync(resolve(E2E, evidence.owner), 'utf8').replace(/\s+/g, '');
    const directNeedle = evidence.sdkRoot === 'beta'
      ? `.beta.${evidence.relativeMethod}(`
      : `client.${evidence.relativeMethod}(`;
    if (evidence.sdkHelper) {
      const [helperOwner, helperName] = evidence.sdkHelper.split('#');
      assert.ok(helperOwner && helperName, `${evidence.sdkMethod}: malformed helper evidence`);
      assert.ok(
        source.includes(helperOwner) && source.includes(`${helperName}(`),
        `${evidence.sdkMethod}: ${evidence.owner} does not invoke ${evidence.sdkHelper}`,
      );
      const helperSource = readFileSync(resolve(E2E, helperOwner), 'utf8').replace(/\s+/g, '');
      assert.ok(
        helperSource.includes(`function${helperName}(`)
          && helperSource.includes(directNeedle),
        `${evidence.sdkMethod}: ${evidence.sdkHelper} does not invoke the official SDK method`,
      );
    } else {
      assert.ok(
        source.includes(directNeedle),
        `${evidence.sdkMethod}: ${evidence.owner} does not invoke the official SDK method`,
      );
    }
    assert.ok(
      orchestration.includes(evidence.owner),
      `${evidence.sdkMethod}: ${evidence.owner} is not in the deterministic execution graph`,
    );
  }
});

test('Managed SDK method ownership rejects missing and overlapping scoped entries', () => {
  // Cause/effect graph: C1 one official root-qualified method is missing; C2
  // one is owned twice. Effects: E1 C1 fails exact surface equality; E2 C2
  // fails uniqueness before a duplicate owner can mask drift. Constraint: this
  // mutates copies of the one production manifest, never a second inventory.
  // Decision rules: R2 C1->E1; R3 C2->E2.
  const client = new Anthropic({ apiKey: 'surface-inventory' }); // awaken-allow: secret
  const actual = officialManagedMethods(client);
  assert.throws(
    () => assertExactMethodInventory(MANAGED_TS_METHOD_MANIFEST.slice(1), actual),
    /new\/removed SDK methods/,
    'R2/E1',
  );
  assert.throws(
    () => assertExactMethodInventory(
      [...MANAGED_TS_METHOD_MANIFEST, MANAGED_TS_METHOD_MANIFEST[0]],
      actual,
    ),
    /exactly once/,
    'R3/E2',
  );
});

test('one behavior-owner graph projects the exact wire contract of each SDK version', () => {
  // Metamorphic version projection: C1 0.121 and 0.122 retain the same operation
  // identities/owners; C2 0.122 removes the Files/Skills feature betas from its
  // generated requests. E1 current projection is byte-equivalent to the
  // generated manifest; E2 candidate projection changes only generated wire
  // coordinates and preserves every owner. This lets one real-process suite
  // replay both versions without freezing 0.121 headers into 0.122 evidence.
  const currentOperations = extractOperationsFromPackageRoot(
    resolveSdkPackage('@anthropic-ai/sdk-current').root,
    scope,
  ).operations;
  const candidateOperations = extractOperationsFromPackageRoot(
    resolveSdkPackage('@anthropic-ai/sdk-candidate').root,
    scope,
  ).operations;
  const current = managedTsMethodManifestForOperations(currentOperations);
  const candidate = managedTsMethodManifestForOperations(candidateOperations);
  assert.deepEqual(current, MANAGED_TS_METHOD_MANIFEST, 'C1/E1');
  assert.deepEqual(
    candidate.map(({ sdkMethod, owner }) => ({ sdkMethod, owner })),
    current.map(({ sdkMethod, owner }) => ({ sdkMethod, owner })),
    'C1/E2',
  );
  assert.deepEqual(
    current.find(({ sdkMethod }) => sdkMethod === 'beta.files.delete').betas,
    ['files-api-2025-04-14'],
    'C2',
  );
  assert.deepEqual(
    candidate.find(({ sdkMethod }) => sdkMethod === 'beta.files.delete').betas,
    [],
    'C2/E2',
  );
});

test('wire projection rejects missing, extra, and duplicate operation identities', () => {
  // Fault table: the behavior graph is reusable only when the selected exact
  // SDK has the same closed operation identity set. Missing, newly added, and
  // duplicate identities must fail before any real-process scenario starts.
  const operations = extractOperationsFromPackageRoot(
    resolveSdkPackage('@anthropic-ai/sdk-current').root,
    scope,
  ).operations;
  assert.throws(
    () => managedTsMethodManifestForOperations(operations.slice(1)),
    /exact qualified Managed operation identities/u,
  );
  assert.throws(
    () => managedTsMethodManifestForOperations([
      ...operations,
      { ...operations[0], id: 'beta.future.create' },
    ]),
    /exact qualified Managed operation identities/u,
  );
  assert.throws(
    () => managedTsMethodManifestForOperations([...operations, operations[0]]),
    /is duplicated/u,
  );
});

test('historical wire projection admits only the exact official operation subset', () => {
  // Monotonic-version relation: the oldest supported SDK owns 99 identities
  // from the current 127-operation graph. E1 every historical identity retains
  // its one owner and exact historical wire coordinates; E2 current-only owner
  // families disappear; E3 an unknown identity still fails closed. This reuses
  // the behavior graph without treating a candidate removal as historical.
  const oldest = extractOperationsFromPackageRoot(
    resolveSdkPackage('@anthropic-ai/sdk-oldest').root,
    scope,
  ).operations;
  const current = extractOperationsFromPackageRoot(
    resolveSdkPackage('@anthropic-ai/sdk-current').root,
    scope,
  ).operations;
  const projected = managedTsMethodManifestForOperations(oldest, {
    allowHistoricalSubset: true,
  });
  assert.equal(projected.filter(({ method }) => method).length, 99, 'E1');
  assert.deepEqual(
    projected.filter(({ method }) => method).map(({ sdkMethod }) => sdkMethod).sort(),
    oldest.map(({ id }) => id).sort(),
    'E1',
  );
  const oldestIDs = new Set(oldest.map(({ id }) => id));
  const currentOnlyIDs = new Set(current
    .map(({ id }) => id)
    .filter((id) => !oldestIDs.has(id)));
  assert.ok(currentOnlyIDs.size > 0, 'E2 requires a non-empty version delta');
  assert.ok(
    !projected.some(({ sdkMethod }) => currentOnlyIDs.has(sdkMethod)),
    'E2: current-only operations cannot leak through historical owner reuse',
  );
  assert.throws(
    () => managedTsMethodManifestForOperations([
      ...oldest,
      { ...oldest[0], id: 'beta.future.create' },
    ], { allowHistoricalSubset: true }),
    /subset of the qualified Managed identities/u,
    'E3',
  );
});
