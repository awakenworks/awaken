// Build a standalone distribution of the canonical projection, never a second reducer.
import { createHash } from 'node:crypto';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, resolve, sep } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';
import ts from 'typescript';

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = resolve(packageRoot, '../..');
const oracleRoot = resolve(packageRoot, '../managed-sdk-oracle');
const sdkTypes = new Map([
  ['BetaManagedAgentsSession', 'sessions'],
  ['BetaManagedAgentsSessionEvent', 'events'],
  ['BetaManagedAgentsStreamSessionEvents', 'events'],
]);

export function portableArtifacts(manifest, selectedSdk, files, sourceDigest) {
  const version = /^npm:@anthropic-ai\/sdk@(\d+\.\d+\.\d+)$/.exec(selectedSdk)?.[1];
  if (!version || !/^[a-f0-9]{64}$/.test(sourceDigest)) throw Error('Exact selected SDK and source digest are required');
  const declarations = ts.createSourceFile('index.d.ts', files.declarations, ts.ScriptTarget.Latest, true);
  const imports = declarations.statements.filter(node => ts.isImportDeclaration(node)
    && node.moduleSpecifier.text === '@awaken/managed-sdk-oracle/current-types');
  if (imports.length !== 1) throw Error('Expected one canonical current-types import');
  const names = imports[0].importClause?.namedBindings;
  if (!names || !ts.isNamedImports(names)) throw Error('Canonical current-types import must be named');
  const grouped = new Map();
  for (const item of names.elements) {
    const family = sdkTypes.get(item.name.text);
    if (!family || item.propertyName) throw Error('Unmapped canonical SDK type: ' + item.name.text);
    grouped.set(family, [...(grouped.get(family) ?? []), item.name.text]);
  }
  const publicTypes = [...grouped].map(([family, values]) =>
    `import type { ${values.join(', ')} } from "@anthropic-ai/sdk/resources/beta/sessions/${family}";`).join('\n');
  const typeImport = imports[0];
  const rewritten = files.declarations.slice(0, typeImport.getStart(declarations)) + publicTypes + files.declarations.slice(typeImport.end);
  const localWire = text => text.replaceAll('@awaken/managed-sdk-oracle/current-wire', './current-wire.js');
  const result = {
    'dist/index.js': localWire(files.runtime),
    'dist/index.d.ts': localWire(rewritten),
    'dist/current-wire.js': files.wireRuntime,
    'dist/current-wire.d.ts': files.wireDeclarations,
  };
  for (const [name, value] of Object.entries(result)) {
    const parsed = ts.createSourceFile(name, value, ts.ScriptTarget.Latest, true);
    const visit = node => {
      const module = ts.isImportDeclaration(node) || ts.isExportDeclaration(node) ? node.moduleSpecifier
        : ts.isImportTypeNode(node) && ts.isLiteralTypeNode(node.argument) ? node.argument.literal
          : ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword ? node.arguments[0] : undefined;
      const allowed = name.endsWith('.d.ts') ? ['./current-wire.js', '@anthropic-ai/sdk/resources/beta/sessions/events', '@anthropic-ai/sdk/resources/beta/sessions/sessions'] : ['./current-wire.js'];
      if (module && (!ts.isStringLiteral(module) || !allowed.includes(module.text))) throw Error('Non-portable artifact: ' + name);
      ts.forEachChild(node, visit);
    };
    visit(parsed);
    if (/[A-Z]:\\|\/build\/scratch\//.test(value)) throw Error('Machine path in artifact: ' + name);
  }
  const distribution = {
    name: manifest.name, version: manifest.version, description: manifest.description,
    license: manifest.license, private: true, type: 'module', files: ['dist', 'README.md', 'TESTING.md', 'LICENSE'],
    exports: { '.': { types: './dist/index.d.ts', import: './dist/index.js' } },
    dependencies: { '@anthropic-ai/sdk': version }, engines: { node: '>=22' },
  };
  return { ...result, 'package.json': JSON.stringify(distribution, null, 2) + '\n',
    'dist/provenance.json': JSON.stringify({ package: manifest.name, version: manifest.version, sdk: version, sourceInputsSha256: sourceDigest }, null, 2) + '\n' };
}

export function packageManager(args, cwd, options = {}) {
  const entry = process.env.npm_execpath;
  if (!entry || !isAbsolute(entry) || !/pnpm\.(?:cjs|mjs|js)$/.test(entry)) throw Error('Run through the pinned pnpm package script');
  const result = spawnSync(process.execPath, [entry, ...args], { cwd, encoding: 'utf8', maxBuffer: 8 * 1024 * 1024, ...options });
  if (result.error || result.status !== 0) throw Error(`${args[0]} failed: ${result.error?.message ?? ''}\n${result.stderr ?? ''}${result.stdout ?? ''}`);
  return result.stdout;
}

export function removeOwnedTemporary(root, prefix) {
  const parent = resolve(tmpdir());
  if (!resolve(root).startsWith(parent + sep) || !dirname(resolve(root)).startsWith(parent)
    || !resolve(root).slice(parent.length + 1).startsWith(prefix)) throw Error('Refusing cleanup outside the owned temporary directory');
  rmSync(root, { recursive: true, force: true });
}

export function packPortable(output, { build = true } = {}) {
  const destination = resolve(output);
  if (destination === repositoryRoot || destination.startsWith(repositoryRoot + sep)) throw Error('Package artifacts must use an output directory outside the checkout');
  if (build) {
    packageManager(['--filter', '@awaken/managed-sdk-oracle', 'exec', 'node', 'src/cli.mjs', 'check'], repositoryRoot);
    packageManager(['--filter', '@awaken/managed-sdk-oracle', 'build'], repositoryRoot);
    packageManager(['--filter', '@awaken/managed-session-projection', 'build'], repositoryRoot);
  }
  const sourcePaths = [join(packageRoot, 'src/index.ts'), join(packageRoot, 'package.json'), fileURLToPath(import.meta.url),
    join(oracleRoot, 'src/current-wire.generated.ts'), join(oracleRoot, 'src/current-types.ts'), join(oracleRoot, 'package.json')];
  const digest = createHash('sha256');
  for (const file of sourcePaths) { const bytes = readFileSync(file); digest.update(String(bytes.length) + ':'); digest.update(bytes); }
  const artifacts = portableArtifacts(JSON.parse(readFileSync(join(packageRoot, 'package.json'), 'utf8')),
    JSON.parse(readFileSync(join(oracleRoot, 'package.json'), 'utf8')).dependencies['@anthropic-ai/sdk-current'], {
      runtime: readFileSync(join(packageRoot, 'dist/index.js'), 'utf8'),
      declarations: readFileSync(join(packageRoot, 'dist/index.d.ts'), 'utf8'),
      wireRuntime: readFileSync(join(oracleRoot, 'dist/current-wire.generated.js'), 'utf8'),
      wireDeclarations: readFileSync(join(oracleRoot, 'dist/current-wire.generated.d.ts'), 'utf8'),
    }, digest.digest('hex'));
  const stage = mkdtempSync(join(tmpdir(), 'awaken-projection-pack-'));
  try {
    for (const [file, text] of Object.entries(artifacts)) { mkdirSync(dirname(join(stage, file)), { recursive: true }); writeFileSync(join(stage, file), text); }
    for (const file of ['README.md', 'TESTING.md', 'LICENSE']) writeFileSync(join(stage, file), readFileSync(file === 'LICENSE' ? join(repositoryRoot, file) : join(packageRoot, file)));
    mkdirSync(destination, { recursive: true });
    packageManager(['pack', '--pack-destination', destination], stage);
    return join(destination, `awaken-managed-session-projection-${JSON.parse(artifacts['package.json']).version}.tgz`);
  } finally { removeOwnedTemporary(stage, 'awaken-projection-pack-'); }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  if (process.argv.length !== 4 || process.argv[2] !== '--out-dir') throw Error('usage: pack-portable.mjs --out-dir <directory outside checkout>');
  console.log(packPortable(process.argv[3]));
}
