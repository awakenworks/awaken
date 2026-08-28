import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

import { resolveSdkPackage, sdkPackageFromRoot } from './package-source.mjs';
import { stableJson } from './normalize.mjs';

const declarationPattern = /\bexport\s+(?:async\s+)?(?:class|function|const|let|var)\s+([A-Za-z_$][\w$]*)/gu;
const clausePattern = /\bexport\s*\{([^}]+)\}(?:\s+from\s+['"][^'"]+['"])?\s*;/gu;

export function staticEsmExports(source, sourceName = 'module') {
  if (/\bexport\s*\*\s*from\b/u.test(source)) {
    throw new Error(`${sourceName}: export * cannot prove an exact Managed helper surface`);
  }
  const names = new Set([...source.matchAll(declarationPattern)].map((match) => match[1]));
  for (const match of source.matchAll(clausePattern)) {
    for (const raw of match[1].split(',')) {
      const member = raw.trim();
      if (!member) continue;
      const parts = member.split(/\s+as\s+/u);
      const exported = parts.at(-1)?.trim();
      if (!/^[A-Za-z_$][\w$]*$/u.test(exported ?? '')) {
        throw new Error(`${sourceName}: unsupported export member ${JSON.stringify(member)}`);
      }
      names.add(exported);
    }
  }
  return [...names].sort();
}

export function managedExportFingerprint(moduleName, scope, options) {
  return managedExportFingerprintFromPackageRoot(
    resolveSdkPackage(moduleName).root,
    scope,
    options,
  );
}

export function managedExportFingerprintFromPackageRoot(
  packageRoot,
  scope,
  { allowMissing = false } = {},
) {
  sdkPackageFromRoot(packageRoot);
  const entrypoints = scope.managed_export_entrypoints ?? [];
  if (entrypoints.length === 0) throw new Error('Managed export entrypoints are not configured');
  const exports = entrypoints.flatMap((entrypoint) => {
    const filename = path.join(packageRoot, entrypoint);
    if (!fs.existsSync(filename) || !fs.statSync(filename).isFile()) {
      if (allowMissing) return [];
      throw new Error(`${entrypoint}: Managed helper entrypoint is missing`);
    }
    return staticEsmExports(fs.readFileSync(filename, 'utf8'), entrypoint).map((name) => Object.freeze({
      id: `${entrypoint}#${name}`,
      entrypoint,
      name,
    }));
  }).sort((left, right) => left.id.localeCompare(right.id));
  if (new Set(exports.map(({ id }) => id)).size !== exports.length) {
    throw new Error('Managed helper export identities must be unique');
  }
  return Object.freeze({
    fingerprint: crypto.createHash('sha256')
      .update(JSON.stringify(stableJson(exports)))
      .digest('hex'),
    export_count: exports.length,
    exports: Object.freeze(exports),
  });
}
