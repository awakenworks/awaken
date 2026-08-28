import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import ts from 'typescript';

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

function declarationSource(root, relative) {
  const filename = path.join(root, relative);
  return { text: fs.readFileSync(filename, 'utf8') };
}

function declarationContext(root, relative) {
  const filename = path.join(root, relative);
  const program = ts.createProgram([filename], {
    module: ts.ModuleKind.NodeNext,
    moduleResolution: ts.ModuleResolutionKind.NodeNext,
    noEmit: true,
    skipLibCheck: true,
    target: ts.ScriptTarget.Latest,
  });
  const source = program.getSourceFile(filename);
  if (!source) throw new Error(`${filename}: TypeScript did not load the SDK declaration`);
  return { filename, source, checker: program.getTypeChecker() };
}

function oneDeclaration(context, name, predicate) {
  const matches = context.source.statements.filter(
    (node) => predicate(node) && node.name.text === name,
  );
  if (matches.length !== 1) {
    throw new Error(`${context.filename}: expected one ${name}, found ${matches.length}`);
  }
  return matches[0];
}

function interfaceProperties(context, interfaceName) {
  const declaration = oneDeclaration(context, interfaceName, ts.isInterfaceDeclaration);
  const symbol = context.checker.getSymbolAtLocation(declaration.name);
  if (!symbol) throw new Error(`${context.filename}: ${interfaceName} has no type symbol`);
  const type = context.checker.getDeclaredTypeOfSymbol(symbol);
  const required = [];
  const optional = [];
  for (const property of context.checker.getPropertiesOfType(type)) {
    const target = property.flags & ts.SymbolFlags.Optional ? optional : required;
    target.push(property.getName());
  }
  return { required: required.sort(), optional: optional.sort() };
}

function unionDiscriminants(context, aliasName) {
  const declaration = oneDeclaration(context, aliasName, ts.isTypeAliasDeclaration);
  const union = context.checker.getTypeAtLocation(declaration);
  const variants = union.isUnion() ? union.types : [union];
  const values = new Set();
  for (const variant of variants) {
    const property = variant.getProperty('type');
    const propertyDeclaration = property?.valueDeclaration ?? property?.declarations?.[0];
    if (!property || !propertyDeclaration) {
      throw new Error(`${context.filename}: ${aliasName} variant has no type discriminator`);
    }
    const discriminator = context.checker.getTypeOfSymbolAtLocation(
      property,
      propertyDeclaration,
    );
    const literals = discriminator.isUnion() ? discriminator.types : [discriminator];
    for (const literal of literals) {
      if (!literal.isStringLiteral()) {
        throw new Error(
          `${context.filename}: ${aliasName} has non-literal discriminator ${context.checker.typeToString(literal)}`,
        );
      }
      values.add(literal.value);
    }
  }
  return [...values].sort();
}

function literalEventTypes(root, relative) {
  const context = declarationContext(root, relative);
  const outbound = unionDiscriminants(context, 'BetaManagedAgentsSessionEvent');
  const inbound = unionDiscriminants(context, 'BetaManagedAgentsEventParams');
  const stream = unionDiscriminants(context, 'BetaManagedAgentsStreamSessionEvents');
  const outboundSet = new Set(outbound);
  const preview = stream.filter((value) => !outboundSet.has(value));
  if (inbound.some((value) => !outboundSet.has(value))) {
    throw new Error(`${context.filename}: inbound event is absent from committed history`);
  }
  return { outbound, inbound, preview };
}

export function managedWireContract(moduleName) {
  const sdk = resolveSdkPackage(moduleName);
  const sessions = declarationContext(
    sdk.root,
    'resources/beta/sessions/sessions.d.ts',
  );
  const events = declarationSource(
    sdk.root,
    'resources/beta/sessions/events.d.ts',
  );
  const beta = declarationSource(sdk.root, 'resources/beta/beta.d.ts');
  const managedBetas = new Set();
  for (const { text } of [beta, events]) {
    for (const match of text.matchAll(/managed-agents-\d{4}-\d{2}-\d{2}/gu)) {
      managedBetas.add(match[0]);
    }
  }
  return {
    session: {
      ...interfaceProperties(sessions, 'BetaManagedAgentsSession'),
      nested: {
        agent: interfaceProperties(sessions, 'BetaManagedAgentsSessionAgent'),
      },
    },
    events: literalEventTypes(sdk.root, 'resources/beta/sessions/events.d.ts'),
    managed_betas: [...managedBetas].sort(),
  };
}
