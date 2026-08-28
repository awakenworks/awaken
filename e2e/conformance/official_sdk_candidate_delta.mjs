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
import { stableJson } from '../../packages/managed-sdk-oracle/src/normalize.mjs';

const deltaKinds = Object.freeze([
  ['operations', 'id'],
  ['declarations', 'path'],
  ['runtime', 'path'],
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
  return Object.freeze({
    currentVersion: currentOperations.version,
    candidateVersion: candidateOperations.version,
    operations: changedEntries(currentOperations.operations, candidateOperations.operations, 'id'),
    declarations: changedEntries(currentTypes.files, candidateTypes.files, 'path'),
    runtime: changedEntries(currentRuntime.files, candidateRuntime.files, 'path'),
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
  return Object.freeze({
    requiredBehaviorOwners: Object.freeze(requiredBehaviorOwners),
  });
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
  const supportedDeclaration = /^beta\/(?:files(?:\/|\.d\.ts$)|skills(?:\/|\.d\.ts$)|webhooks\.d\.ts$)/u;
  const unsupportedDeclaration = delta.declarations.changed.find(
    ({ path }) => !supportedDeclaration.test(path),
  );
  if (unsupportedDeclaration) {
    throw new Error(`${unsupportedDeclaration.path} changed outside the latest runtime canary`);
  }
  if (delta.runtime.added.length > 0 || delta.runtime.removed.length > 0) {
    throw new Error('candidate adds or removes Managed runtime dependencies; add explicit behavior ownership');
  }
  const runtimeOwners = [
    /^(?:core\/middleware|internal\/(?:errors|parse|uploads))\.mjs$/u,
    /^lib\/sessions\/accumulate\.mjs$/u,
    /^resources\/beta\/(?:files|webhooks)\.mjs$/u,
    /^resources\/beta\/skills\/(?:skills|versions)\.mjs$/u,
    /^tools\/agent-toolset\/(?:node|skills)\.mjs$/u,
    /^version\.mjs$/u,
  ];
  const unsupportedRuntime = delta.runtime.changed.find(
    ({ path }) => !runtimeOwners.some((owner) => owner.test(path)),
  );
  if (unsupportedRuntime) {
    throw new Error(`${unsupportedRuntime.path} changed outside the latest runtime canary`);
  }
}
