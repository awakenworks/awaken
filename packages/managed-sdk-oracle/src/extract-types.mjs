import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import ts from 'typescript';

import { resolveSdkPackage, sdkPackageFromRoot } from './package-source.mjs';

function declarations(root) {
  if (!fs.existsSync(root)) return [];
  if (fs.statSync(root).isFile()) return root.endsWith('.d.ts') ? [root] : [];
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
  return managedTypeFingerprintFromPackageRoot(sdk.root, scope);
}

export function managedTypeFingerprintFromPackageRoot(root, scope) {
  sdkPackageFromRoot(root);
  const betaRoot = path.join(root, 'resources', 'beta');
  const roots = [
    { root: betaRoot, prefix: 'beta/', allowed: new Set(scope.beta_resource_roots) },
    { root: path.join(root, 'resources'), prefix: 'ga/', allowed: new Set(scope.ga_resource_roots ?? []) },
  ];
  const hash = crypto.createHash('sha256');
  const files = [];
  for (const { root: declarationRoot, prefix, allowed } of roots) {
    for (const filename of declarations(declarationRoot)) {
      const relative = path.relative(declarationRoot, filename).replaceAll(path.sep, '/');
      if (!allowed.has(relative.split('/')[0].replace(/\.d\.ts$/, ''))) continue;
      const scopedPath = prefix + relative;
      const content = fs.readFileSync(filename, 'utf8').replaceAll('\r\n', '\n');
      hash.update(scopedPath);
      hash.update('\0');
      hash.update(content);
      hash.update('\0');
      files.push(Object.freeze({
        path: scopedPath,
        fingerprint: crypto.createHash('sha256').update(content).digest('hex'),
      }));
    }
  }
  for (const configuredRoot of scope.managed_declaration_roots ?? []) {
    const declarationRoot = path.join(root, configuredRoot);
    for (const filename of declarations(declarationRoot)) {
      const relative = path.relative(root, filename).replaceAll(path.sep, '/');
      const scopedPath = `managed/${relative}`;
      const content = fs.readFileSync(filename, 'utf8').replaceAll('\r\n', '\n');
      hash.update(scopedPath);
      hash.update('\0');
      hash.update(content);
      hash.update('\0');
      files.push(Object.freeze({
        path: scopedPath,
        fingerprint: crypto.createHash('sha256').update(content).digest('hex'),
      }));
    }
  }
  if (files.length === 0) throw new Error(`${root} exposed no scoped Managed declarations`);
  return Object.freeze({
    fingerprint: hash.digest('hex'),
    file_count: files.length,
    files: Object.freeze(files),
  });
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

/// One object type's wire shape: its required and optional property names and,
/// for any property whose type is a discriminated union of objects, the shape
/// of each variant keyed by its `type` literal. Depth-bounded so a nested
/// object such as `evaluation.evaluated_permission` is described, while the
/// open-ended content-block trees are left to the block extractors.
function objectShape(context, type, depth) {
  const required = [];
  const optional = [];
  const nested = {};
  for (const property of context.checker.getPropertiesOfType(type)) {
    const name = property.getName();
    (property.flags & ts.SymbolFlags.Optional ? optional : required).push(name);
    if (depth === 0) continue;
    const declaration = property.valueDeclaration ?? property.declarations?.[0];
    if (!declaration) continue;
    const propertyType = context.checker.getTypeOfSymbolAtLocation(property, declaration);
    const members = (propertyType.isUnion() ? propertyType.types : [propertyType]).filter(
      (member) => !(member.flags & (ts.TypeFlags.Undefined | ts.TypeFlags.Null)),
    );
    const variants = discriminatedShapes(context, members, depth - 1);
    if (variants) nested[name] = variants;
  }
  const shape = { required: required.sort(), optional: optional.sort() };
  if (Object.keys(nested).length > 0) shape.nested = nested;
  return shape;
}

/// The variants of a discriminated object union keyed by `type` literal, or
/// `null` when the members are not all objects carrying a literal `type`.
function discriminatedShapes(context, members, depth) {
  const variants = {};
  for (const member of members) {
    if (!(member.flags & ts.TypeFlags.Object)) return null;
    const tag = member.getProperty('type');
    const tagDeclaration = tag?.valueDeclaration ?? tag?.declarations?.[0];
    if (!tag || !tagDeclaration) return null;
    const tagType = context.checker.getTypeOfSymbolAtLocation(tag, tagDeclaration);
    const literals = tagType.isUnion() ? tagType.types : [tagType];
    if (!literals.every((literal) => literal.isStringLiteral())) return null;
    for (const literal of literals) {
      variants[literal.value] = objectShape(context, member, depth);
    }
  }
  return Object.keys(variants).length > 0 ? variants : null;
}

/// Every committed event's property shape keyed by its `type` literal. The
/// discriminator list says which events exist; this says what each carries,
/// so a field added to an existing event — an optional `evaluation`, a new
/// required `usage` — is a visible contract change, not a silent one.
function eventShapes(context) {
  const declaration = oneDeclaration(context, 'BetaManagedAgentsSessionEvent', ts.isTypeAliasDeclaration);
  const union = context.checker.getTypeAtLocation(declaration);
  const members = union.isUnion() ? union.types : [union];
  const shapes = discriminatedShapes(context, members, 2);
  if (!shapes) {
    throw new Error(`${context.filename}: BetaManagedAgentsSessionEvent is not a discriminated object union`);
  }
  return shapes;
}

/// The exported declaration names of every scoped Managed `.d.ts` file: the
/// vocabulary a wire type may cite as its SDK counterpart.
export function managedDeclaredTypesFromPackageRoot(root, scope) {
  const names = new Set();
  for (const { path: scopedPath } of managedTypeFingerprintFromPackageRoot(root, scope).files) {
    const [prefix, ...rest] = scopedPath.split('/');
    const relative = prefix === 'beta' ? path.join('resources', 'beta', ...rest)
      : prefix === 'ga' ? path.join('resources', ...rest)
        : rest.join('/');
    const text = fs.readFileSync(path.join(root, relative), 'utf8');
    for (const match of text.matchAll(/^export (?:interface|type|declare (?:const|function|class|abstract class)|class) ([A-Za-z0-9_]+)/gmu)) {
      names.add(match[1]);
    }
  }
  return [...names].sort();
}

export function managedDeclaredTypes(moduleName, scope) {
  return managedDeclaredTypesFromPackageRoot(resolveSdkPackage(moduleName).root, scope);
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
  const shapes = eventShapes(context);
  const unshaped = outbound.filter((value) => !(value in shapes));
  if (unshaped.length > 0) {
    throw new Error(`${context.filename}: committed events without a shape: ${unshaped.join(', ')}`);
  }
  return { outbound, inbound, preview, shapes };
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
  const pagination = declarationContext(sdk.root, 'core/pagination.d.ts');
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
    // The list-envelope and list-query shapes every Managed collection shares.
    // Keyed by the SDK interface each Rust wire type mirrors, so a field the
    // SDK adds, drops, or makes optional reaches the conformance test as a
    // named difference rather than a silent client parse failure.
    pagination: {
      PageCursorResponse: interfaceProperties(pagination, 'PageCursorResponse'),
      PageCursorParams: interfaceProperties(pagination, 'PageCursorParams'),
      PageResponse: interfaceProperties(pagination, 'PageResponse'),
    },
    resource_params: {
      file: interfaceProperties(sessions, 'BetaManagedAgentsFileResourceParams'),
      github_repository: interfaceProperties(
        sessions,
        'BetaManagedAgentsGitHubRepositoryResourceParams',
      ),
      memory_store: interfaceProperties(
        sessions,
        'BetaManagedAgentsMemoryStoreResourceParam',
      ),
    },
  };
}
