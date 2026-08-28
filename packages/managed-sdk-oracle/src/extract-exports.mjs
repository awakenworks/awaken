import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import ts from 'typescript';

import { resolveSdkPackage, sdkPackageFromRoot } from './package-source.mjs';
import { stableJson } from './normalize.mjs';

function hasModifier(node, kind) {
  return node.modifiers?.some((modifier) => modifier.kind === kind) ?? false;
}

function collectBindingNames(binding, names) {
  if (ts.isIdentifier(binding)) {
    names.add(binding.text);
    return;
  }
  for (const element of binding.elements) {
    if (!ts.isOmittedExpression(element)) collectBindingNames(element.name, names);
  }
}

export function staticEsmExports(source, sourceName = 'module') {
  const module = ts.createSourceFile(
    sourceName,
    source,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.JS,
  );
  if (module.parseDiagnostics.length > 0) {
    const messages = module.parseDiagnostics
      .map(({ messageText }) => ts.flattenDiagnosticMessageText(messageText, '\n'))
      .join('; ');
    throw new Error(`${sourceName}: cannot parse Managed helper exports: ${messages}`);
  }

  const names = new Set();
  for (const statement of module.statements) {
    if (ts.isExportDeclaration(statement)) {
      if (!statement.exportClause) {
        throw new Error(`${sourceName}: export * cannot prove an exact Managed helper surface`);
      }
      if (ts.isNamespaceExport(statement.exportClause)) {
        names.add(statement.exportClause.name.text);
      } else {
        for (const element of statement.exportClause.elements) names.add(element.name.text);
      }
      continue;
    }
    if (ts.isExportAssignment(statement)) {
      if (statement.isExportEquals) {
        throw new Error(`${sourceName}: export = is not an ESM helper surface`);
      }
      names.add('default');
      continue;
    }
    if (!hasModifier(statement, ts.SyntaxKind.ExportKeyword)) continue;
    if (hasModifier(statement, ts.SyntaxKind.DefaultKeyword)) {
      names.add('default');
      continue;
    }
    if (ts.isVariableStatement(statement)) {
      for (const declaration of statement.declarationList.declarations) {
        collectBindingNames(declaration.name, names);
      }
      continue;
    }
    if ((ts.isFunctionDeclaration(statement) || ts.isClassDeclaration(statement))
      && statement.name) {
      names.add(statement.name.text);
      continue;
    }
    throw new Error(`${sourceName}: unsupported exported declaration ${statement.kind}`);
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
