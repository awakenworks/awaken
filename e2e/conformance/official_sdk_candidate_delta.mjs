import crypto from 'node:crypto';

import {
  extractOperationsFromPackageRoot,
} from '../../packages/managed-sdk-oracle/src/extract-operations.mjs';
import {
  managedTypeFingerprintFromPackageRoot,
} from '../../packages/managed-sdk-oracle/src/extract-types.mjs';
import {
  managedRuntimeFingerprintFromPackageRoot,
} from '../../packages/managed-sdk-oracle/src/extract-runtime.mjs';
import {
  managedExportFingerprintFromPackageRoot,
} from '../../packages/managed-sdk-oracle/src/extract-exports.mjs';
import { stableJson } from '../../packages/managed-sdk-oracle/src/normalize.mjs';

const deltaKinds = Object.freeze([
  ['operations', 'id'],
  ['declarations', 'path'],
  ['runtime', 'path'],
  ['exports', 'id'],
]);

function changedEntries(current, candidate, key) {
  const before = new Map(current.map((entry) => [entry[key], entry]));
  const after = new Map(candidate.map((entry) => [entry[key], entry]));
  return Object.freeze({
    added: Object.freeze(candidate.filter((entry) => !before.has(entry[key]))),
    removed: Object.freeze(current.filter((entry) => !after.has(entry[key]))),
    changed: Object.freeze(candidate.flatMap((entry) => {
      const previous = before.get(entry[key]);
      return previous && JSON.stringify(previous) !== JSON.stringify(entry)
        ? [Object.freeze({ [key]: entry[key], before: previous, after: entry })]
        : [];
    })),
  });
}

export function officialSdkCandidateDelta(currentRoot, candidateRoot, scope) {
  const currentOperations = extractOperationsFromPackageRoot(currentRoot, scope);
  const candidateOperations = extractOperationsFromPackageRoot(candidateRoot, scope);
  const currentTypes = managedTypeFingerprintFromPackageRoot(currentRoot, scope);
  const candidateTypes = managedTypeFingerprintFromPackageRoot(candidateRoot, scope);
  const currentRuntime = managedRuntimeFingerprintFromPackageRoot(currentRoot, scope);
  const candidateRuntime = managedRuntimeFingerprintFromPackageRoot(candidateRoot, scope);
  const currentExports = managedExportFingerprintFromPackageRoot(currentRoot, scope);
  const candidateExports = managedExportFingerprintFromPackageRoot(candidateRoot, scope);
  return Object.freeze({
    currentVersion: currentOperations.version,
    candidateVersion: candidateOperations.version,
    operations: changedEntries(currentOperations.operations, candidateOperations.operations, 'id'),
    declarations: changedEntries(currentTypes.files, candidateTypes.files, 'path'),
    runtime: changedEntries(currentRuntime.files, candidateRuntime.files, 'path'),
    exports: changedEntries(currentExports.exports, candidateExports.exports, 'id'),
  });
}

export function officialSdkCandidateDeltaFingerprint(delta) {
  return crypto.createHash('sha256')
    .update(JSON.stringify(stableJson(delta)))
    .digest('hex');
}

export function officialSdkCandidateDeltaCoordinates(delta) {
  return Object.freeze(deltaKinds.flatMap(([kind, key]) => (
    ['added', 'removed', 'changed'].flatMap((change) => (
      delta[kind][change].map((entry) => `${kind}:${change}:${entry[key]}`)
    ))
  )).sort());
}

function exactMembers(actual, expected) {
  return actual.length === expected.length
    && actual.every((value, index) => value === expected[index]);
}

export function qualifyOfficialSdkCandidateDelta(delta, qualifications) {
  assertLatestRuntimeOwnsCandidateDelta(delta);
  const coordinates = officialSdkCandidateDeltaCoordinates(delta);
  if (delta.currentVersion === delta.candidateVersion) {
    if (coordinates.length > 0) {
      throw new Error('same-version SDK package content differs from the generated current anchor');
    }
    return Object.freeze({ requiredBehaviorOwners: Object.freeze([]) });
  }

  if (!Array.isArray(qualifications)) {
    throw new Error('candidate qualification catalog must be an array');
  }
  const matches = qualifications.filter(({ baseline_version, candidate_version }) => (
    baseline_version === delta.currentVersion && candidate_version === delta.candidateVersion
  ));
  if (matches.length !== 1) {
    throw new Error(
      `candidate ${delta.currentVersion} -> ${delta.candidateVersion} requires one exact qualification`,
    );
  }
  const qualification = matches[0];
  if (!Array.isArray(qualification.evidence_groups)
    || qualification.evidence_groups.length === 0
    || qualification.evidence_groups.some(({ coordinates: owned }) => (
      !Array.isArray(owned) || owned.length === 0
        || owned.some((coordinate) => typeof coordinate !== 'string' || coordinate.length === 0)
    ))) {
    throw new Error(`candidate ${delta.candidateVersion} qualification has invalid evidence groups`);
  }
  const fingerprint = officialSdkCandidateDeltaFingerprint(delta);
  if (qualification.delta_fingerprint !== fingerprint) {
    throw new Error(
      `candidate ${delta.candidateVersion} delta fingerprint is not the reviewed qualification`,
    );
  }

  const ownedCoordinates = qualification.evidence_groups
    .flatMap(({ coordinates: owned }) => owned)
    .sort();
  if (new Set(ownedCoordinates).size !== ownedCoordinates.length) {
    throw new Error(`candidate ${delta.candidateVersion} qualification owns a coordinate twice`);
  }
  if (!exactMembers(ownedCoordinates, coordinates)) {
    throw new Error(
      `candidate ${delta.candidateVersion} qualification does not own every exact delta coordinate`,
    );
  }
  const requiredBehaviorOwners = qualification.evidence_groups.map(({ owner }) => owner);
  if (requiredBehaviorOwners.some((owner) => typeof owner !== 'string' || owner.length === 0)
    || new Set(requiredBehaviorOwners).size !== requiredBehaviorOwners.length) {
    throw new Error(`candidate ${delta.candidateVersion} qualification has invalid behavior owners`);
  }
  const witnesses = declarationWitnesses(delta.candidateVersion, qualification);
  return Object.freeze({
    requiredBehaviorOwners: Object.freeze(requiredBehaviorOwners),
    witnesses: Object.freeze(witnesses),
  });
}

// A behavior owner may claim a changed declaration file only by naming what it
// read there: a regular expression over that file's new content. Without it a
// changed `.d.ts` can be "owned" by a proof that never opens the file, which is
// how an added event field went unreviewed. Runtime coordinates are owned by
// executing the code; declarations are owned by reading it.
export function declarationWitnesses(candidateVersion, qualification) {
  const witnesses = [];
  for (const group of qualification.evidence_groups) {
    const declared = group.witnesses ?? {};
    for (const coordinate of group.coordinates) {
      if (!coordinate.startsWith('declarations:')) continue;
      const pattern = declared[coordinate];
      if (typeof pattern !== 'string' || pattern.length === 0) {
        throw new Error(
          `candidate ${candidateVersion}: ${group.owner} owns ${coordinate} without a witness pattern`,
        );
      }
      let regexp;
      try {
        regexp = new RegExp(pattern, 'u');
      } catch (error) {
        throw new Error(`candidate ${candidateVersion}: ${coordinate} witness is not a regular expression: ${error.message}`);
      }
      witnesses.push(Object.freeze({ owner: group.owner, coordinate, regexp }));
    }
    for (const coordinate of Object.keys(declared)) {
      if (!group.coordinates.includes(coordinate)) {
        throw new Error(`candidate ${candidateVersion}: ${group.owner} witnesses ${coordinate} it does not own`);
      }
    }
  }
  return witnesses;
}

// Resolve a declaration coordinate to the file it names inside a package root,
// the inverse of the scoped path the type fingerprint assigns.
export function declarationCoordinatePath(coordinate) {
  const scoped = coordinate.replace(/^declarations:(?:added|changed|removed):/u, '');
  const [prefix, ...rest] = scoped.split('/');
  if (prefix === 'beta') return ['resources', 'beta', ...rest].join('/');
  if (prefix === 'ga') return ['resources', ...rest].join('/');
  if (prefix === 'managed') return rest.join('/');
  throw new Error(`${coordinate} is not a scoped declaration coordinate`);
}

// Every witness must match the candidate's actual file, or the review named a
// change that is not there.
export function assertDeclarationWitnesses(witnesses, readCandidateFile) {
  for (const { owner, coordinate, regexp } of witnesses) {
    const content = readCandidateFile(declarationCoordinatePath(coordinate));
    if (!regexp.test(content)) {
      throw new Error(`${owner}: witness ${regexp} for ${coordinate} does not match the candidate declaration`);
    }
  }
}

// The qualification catalog also retains regression obligations after promotion.
// Walk only incoming upgrade edges for the selected version; future/unrelated
// candidates cannot require unavailable features, and a shared owner runs once.
export function assertRetainedOfficialSdkBehaviorOwners(version, qualifications, completed) {
  const pending = [version];
  const visited = new Set();
  const owners = new Set();
  while (pending.length > 0) {
    const selected = pending.pop();
    if (visited.has(selected)) continue;
    visited.add(selected);
    for (const entry of qualifications.filter(({ candidate_version }) => candidate_version === selected)) {
      pending.push(entry.baseline_version);
      for (const { owner } of entry.evidence_groups) owners.add(owner);
    }
  }
  for (const owner of owners) {
    if (!completed.has(owner)) throw new Error(`retained SDK behavior owner did not complete: ${owner}`);
  }
  return Object.freeze([...owners].sort());
}

export function assertLatestRuntimeOwnsCandidateDelta(delta) {
  // This canary executes every existing Beta/GA Files and Skills method plus all
  // recognized Webhook helpers. New or removed operations/declaration files need
  // a new explicit behavior owner; only in-place changes inside those three
  // exercised families may reuse this proof.
  if (delta.operations.added.length > 0 || delta.operations.removed.length > 0) {
    throw new Error('candidate adds or removes SDK operations; add explicit behavior ownership');
  }
  const unsupportedOperation = delta.operations.changed.find(
    ({ id }) => !id.startsWith('beta.files.') && !id.startsWith('beta.skills.'),
  );
  if (unsupportedOperation) {
    throw new Error(`${unsupportedOperation.id} changed outside the latest runtime canary`);
  }
  if (delta.declarations.added.length > 0 || delta.declarations.removed.length > 0) {
    throw new Error('candidate adds or removes scoped declaration files; review the SDK scope');
  }
  const supportedDeclaration = /^(?:beta\/.+\.d\.ts|ga\/.+\.d\.ts|managed\/(?:lib\/(?:sessions\/accumulate|environments\/worker)|tools\/agent-toolset\/(?:fs-util|node(?:\.browser)?|skills))\.d\.ts)$/u;
  const unsupportedDeclaration = delta.declarations.changed.find(
    ({ path }) => !supportedDeclaration.test(path),
  );
  if (unsupportedDeclaration) {
    throw new Error(`${unsupportedDeclaration.path} changed outside the latest runtime canary`);
  }
  const recognizedAddedRuntime = new Set(['internal/node.mjs']);
  if (delta.runtime.added.some(({ path }) => !recognizedAddedRuntime.has(path))
    || delta.runtime.removed.length > 0) {
    throw new Error('candidate adds or removes Managed runtime dependencies; add explicit behavior ownership');
  }
  const recognizedRemovedExports = new Set([
    'tools/agent-toolset/node.mjs#resolveSkillVersion',
  ]);
  if (delta.exports.added.length > 0 || delta.exports.changed.length > 0
    || delta.exports.removed.some(({ id }) => !recognizedRemovedExports.has(id))) {
    throw new Error('candidate changes the Managed helper export surface; add explicit behavior ownership');
  }
  const runtimeOwners = [
    /^client\.mjs$/u,
    /^(?:core\/middleware|internal\/(?:errors|parse|uploads))\.mjs$/u,
    /^core\/streaming\.mjs$/u,
    /^core\/credentials\.mjs$/u,
    /^internal\/(?:file-store|node)\.mjs$/u,
    /^lib\/credentials\/(?:credential-chain|identity-token|types|user-oauth)\.mjs$/u,
    /^lib\/middleware\.mjs$/u,
    /^lib\/sessions\/accumulate\.mjs$/u,
    // 0.125: the Environment worker helper runs a work item on the sessions
    // token its secret carries; owned by `candidate.environment-worker-secret-token`.
    /^lib\/environments\/worker\.mjs$/u,
    /^resources\/(?:beta\/.+|files|models|skills(?:\/(?:index|skills|versions))?)\.mjs$/u,
    /^tools\/agent-toolset\/(?:fs-util|memories|node|skills)\.mjs$/u,
    /^version\.mjs$/u,
    // 0.125 dropped the generator banner from every generated module. These
    // core/internal modules are owned by `candidate.generated-module-loadability`,
    // which imports each one from the exact package root; any behavioral
    // change inside them still needs its own owner because the qualification
    // pins the complete content fingerprint.
    /^(?:index|core\/(?:api-promise|error|pagination|resource)|internal\/(?:detect-platform|headers|request-options|shims|utils|utils\/(?:base64|env|log|query|uuid|values)))\.mjs$/u,
  ];
  const unsupportedRuntime = [...delta.runtime.added, ...delta.runtime.changed].find(
    ({ path }) => !runtimeOwners.some((owner) => owner.test(path)),
  );
  if (unsupportedRuntime) {
    throw new Error(`${unsupportedRuntime.path} changed outside the latest runtime canary`);
  }
}
