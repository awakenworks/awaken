import { appendFileSync, readFileSync, realpathSync } from 'node:fs';
import { registerHooks } from 'node:module';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const CANONICAL_PACKAGE = '@anthropic-ai/sdk';
const configuredPackageRoot = process.env.AWAKEN_MANAGED_SDK_PACKAGE_ROOT;
if (!configuredPackageRoot) throw new Error('AWAKEN_MANAGED_SDK_PACKAGE_ROOT is required');
const packageRoot = realpathSync(configuredPackageRoot);
const expectedVersion = process.env.AWAKEN_MANAGED_SDK_PACKAGE_VERSION;
const resolutionFile = process.env.AWAKEN_MANAGED_SDK_RESOLUTION_FILE;
const manifest = JSON.parse(readFileSync(path.join(packageRoot, 'package.json'), 'utf8'));

if (manifest.name !== CANONICAL_PACKAGE) {
  throw new Error(`selected Managed SDK package has unexpected name ${JSON.stringify(manifest.name)}`);
}
if (manifest.version !== expectedVersion) {
  throw new Error(
    `selected Managed SDK ${JSON.stringify(manifest.version)} does not match expected ${JSON.stringify(expectedVersion)}`,
  );
}
if (!resolutionFile) throw new Error('AWAKEN_MANAGED_SDK_RESOLUTION_FILE is required');

const packageParent = pathToFileURL(path.join(packageRoot, 'package.json')).href;
const isCanonicalSpecifier = (specifier) => (
  specifier === CANONICAL_PACKAGE || specifier.startsWith(`${CANONICAL_PACKAGE}/`)
);

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (!isCanonicalSpecifier(specifier)) return nextResolve(specifier, context);

    // Resolve the package as a self-reference from inside the selected exact
    // package. Node therefore applies the package's own exports and import
    // conditions without a copied subpath table or a mutable node_modules link.
    const result = nextResolve(specifier, { ...context, parentURL: packageParent });
    const resolvedPath = realpathSync(fileURLToPath(result.url));
    const relative = path.relative(packageRoot, resolvedPath);
    if (relative.startsWith('..') || path.isAbsolute(relative)) {
      throw new Error(`Managed SDK resolution escaped selected package root: ${result.url}`);
    }
    appendFileSync(resolutionFile, `${JSON.stringify({
      specifier,
      url: pathToFileURL(resolvedPath).href,
      version: manifest.version,
    })}\n`);
    return { ...result, url: pathToFileURL(resolvedPath).href };
  },
});
