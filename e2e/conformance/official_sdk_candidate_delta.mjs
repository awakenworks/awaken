import {
  extractOperationsFromPackageRoot,
} from '../../packages/managed-sdk-oracle/src/extract-operations.mjs';
import {
  managedTypeFingerprintFromPackageRoot,
} from '../../packages/managed-sdk-oracle/src/extract-types.mjs';

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
  return Object.freeze({
    currentVersion: currentOperations.version,
    candidateVersion: candidateOperations.version,
    operations: changedEntries(currentOperations.operations, candidateOperations.operations, 'id'),
    declarations: changedEntries(currentTypes.files, candidateTypes.files, 'path'),
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
}
