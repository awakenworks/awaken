import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

import { resolveSdkPackage } from './package-source.mjs';

function declarations(root) {
  const files = [];
  const visit = (directory) => {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const candidate = path.join(directory, entry.name);
      if (entry.isDirectory()) visit(candidate);
      else if (entry.isFile() && entry.name.endsWith('.d.ts')) files.push(candidate);
    }
  };
  visit(root);
  return files.sort();
}

export function managedTypeFingerprint(moduleName, scope) {
  const sdk = resolveSdkPackage(moduleName);
  const betaRoot = path.join(sdk.root, 'resources', 'beta');
  const allowed = new Set(scope.beta_resource_roots);
  const hash = crypto.createHash('sha256');
  let fileCount = 0;
  for (const filename of declarations(betaRoot)) {
    const relative = path.relative(betaRoot, filename).replaceAll(path.sep, '/');
    if (!allowed.has(relative.split('/')[0].replace(/\.d\.ts$/, ''))) continue;
    hash.update(relative);
    hash.update('\0');
    hash.update(fs.readFileSync(filename, 'utf8').replaceAll('\r\n', '\n'));
    hash.update('\0');
    fileCount += 1;
  }
  if (fileCount === 0) throw new Error(`${moduleName} exposed no scoped Managed declarations`);
  return { fingerprint: hash.digest('hex'), file_count: fileCount };
}
