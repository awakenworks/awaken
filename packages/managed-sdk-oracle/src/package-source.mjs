import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);

export function pathBelongsToRoot(root, candidate) {
  const remainder = path.relative(path.resolve(root), path.resolve(candidate));
  return remainder.length > 0
    && remainder !== '..'
    && !remainder.startsWith(`..${path.sep}`)
    && !path.isAbsolute(remainder);
}

export function sdkPackageFromRoot(root) {
  const manifestPath = path.join(root, 'package.json');
  if (!fs.existsSync(manifestPath)) {
    throw new Error(`${root} is not an @anthropic-ai/sdk package root`);
  }
  const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
  if (manifest.name !== '@anthropic-ai/sdk' || typeof manifest.version !== 'string') {
    throw new Error(`${root} is not an @anthropic-ai/sdk package root`);
  }
  return Object.freeze({ root, version: manifest.version });
}

export function resolveSdkPackage(moduleName) {
  let current = path.dirname(require.resolve(moduleName));
  for (;;) {
    const manifestPath = path.join(current, 'package.json');
    if (fs.existsSync(manifestPath)) {
      const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
      if (manifest.name === '@anthropic-ai/sdk') {
        return sdkPackageFromRoot(current);
      }
    }
    const parent = path.dirname(current);
    if (parent === current) {
      throw new Error(`Cannot find the @anthropic-ai/sdk package root for ${moduleName}`);
    }
    current = parent;
  }
}

export function walkJavaScript(root) {
  const files = [];
  const visit = (directory) => {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const candidate = path.join(directory, entry.name);
      if (entry.isDirectory()) visit(candidate);
      else if (entry.isFile() && entry.name.endsWith('.js')) files.push(candidate);
    }
  };
  visit(root);
  return files.sort();
}
