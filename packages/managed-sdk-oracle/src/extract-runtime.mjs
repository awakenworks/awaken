import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

import {
  pathBelongsToRoot,
  resolveSdkPackage,
  sdkPackageFromRoot,
} from './package-source.mjs';

const importPattern = /(?:from\s*|import\s*(?:\(\s*)?|export\s+[^;]*?from\s*)['"]([^'"]+)['"]/g;

function runtimeFiles(root) {
  if (!fs.existsSync(root)) return [];
  if (fs.statSync(root).isFile()) return root.endsWith('.mjs') ? [root] : [];
  const files = [];
  const visit = (directory) => {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const candidate = path.join(directory, entry.name);
      if (entry.isDirectory()) visit(candidate);
      else if (entry.isFile() && entry.name.endsWith('.mjs')) files.push(candidate);
    }
  };
  visit(root);
  return files.sort();
}

function scopedResource(packageRoot, filename, scope) {
  const relative = path.relative(packageRoot, filename).replaceAll(path.sep, '/');
  const beta = relative.match(/^resources\/beta\/([^/]+?)(?:\.mjs|\/)/u);
  if (beta) return scope.beta_resource_roots.includes(beta[1]);
  const ga = relative.match(/^resources\/([^/]+?)(?:\.mjs|\/)/u);
  return ga ? (scope.ga_resource_roots ?? []).includes(ga[1]) : true;
}

function resolveRuntimeImport(from, specifier) {
  if (!specifier.startsWith('.')) return undefined;
  const base = path.resolve(path.dirname(from), specifier);
  return [base, `${base}.mjs`, path.join(base, 'index.mjs')]
    .find((candidate) => fs.existsSync(candidate) && fs.statSync(candidate).isFile());
}

function runtimeRoots(packageRoot, scope) {
  const roots = (scope.managed_runtime_entrypoints ?? []).map(
    (entrypoint) => path.join(packageRoot, entrypoint),
  );
  for (const resource of scope.beta_resource_roots) {
    roots.push(path.join(packageRoot, 'resources/beta', resource));
    roots.push(path.join(packageRoot, 'resources/beta', `${resource}.mjs`));
  }
  for (const resource of scope.ga_resource_roots ?? []) {
    roots.push(path.join(packageRoot, 'resources', resource));
    roots.push(path.join(packageRoot, 'resources', `${resource}.mjs`));
  }
  return roots.flatMap(runtimeFiles);
}

export function managedRuntimeFingerprint(moduleName, scope) {
  return managedRuntimeFingerprintFromPackageRoot(resolveSdkPackage(moduleName).root, scope);
}

export function managedRuntimeFingerprintFromPackageRoot(packageRoot, scope) {
  sdkPackageFromRoot(packageRoot);
  const pending = runtimeRoots(packageRoot, scope);
  const visited = new Set();
  while (pending.length > 0) {
    const filename = pending.pop();
    if (visited.has(filename) || !scopedResource(packageRoot, filename, scope)) continue;
    visited.add(filename);
    const source = fs.readFileSync(filename, 'utf8');
    for (const match of source.matchAll(importPattern)) {
      const imported = resolveRuntimeImport(filename, match[1]);
      if (imported && pathBelongsToRoot(packageRoot, imported)) pending.push(imported);
    }
  }
  if (visited.size === 0) throw new Error(`${packageRoot} exposed no scoped Managed runtime files`);
  const hash = crypto.createHash('sha256');
  const files = [...visited]
    .sort()
    .map((filename) => {
      const content = fs.readFileSync(filename, 'utf8').replaceAll('\r\n', '\n');
      const runtimePath = path.relative(packageRoot, filename).replaceAll(path.sep, '/');
      hash.update(runtimePath);
      hash.update('\0');
      hash.update(content);
      hash.update('\0');
      return Object.freeze({
        path: runtimePath,
        fingerprint: crypto.createHash('sha256').update(content).digest('hex'),
      });
    });
  return Object.freeze({
    fingerprint: hash.digest('hex'),
    file_count: files.length,
    files: Object.freeze(files),
  });
}
