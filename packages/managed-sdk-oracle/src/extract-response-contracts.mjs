import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import ts from 'typescript';

import { operationNamespace } from './normalize.mjs';
import { sdkPackageFromRoot, walkJavaScript } from './package-source.mjs';

function scopedJavaScriptFiles(root, scope) {
  const select = (resourceRoot, allowed) => walkJavaScript(resourceRoot).filter((filename) => {
    const resource = path.relative(resourceRoot, filename).split(path.sep)[0].replace(/\.js$/u, '');
    return allowed.has(resource);
  });
  const betaRoot = path.join(root, 'resources', 'beta');
  const gaRoot = path.join(root, 'resources');
  return [
    ...select(betaRoot, new Set(scope.beta_resource_roots)).map((filename) => ({
      filename,
      prefix: 'beta',
      resourceRoot: betaRoot,
    })),
    ...select(gaRoot, new Set(scope.ga_resource_roots ?? [])).map((filename) => ({
      filename,
      prefix: '',
      resourceRoot: gaRoot,
    })),
  ];
}

function declarationPath(filename) {
  return filename.replace(/\.js$/u, '.d.ts');
}

function referenceName(node, source) {
  return ts.isTypeReferenceNode(node) ? node.typeName.getText(source).split('.').at(-1) : undefined;
}

function containsReference(node, source, expected) {
  if (referenceName(node, source) === expected) return true;
  if (ts.isUnionTypeNode(node)) {
    return node.types.some((candidate) => containsReference(candidate, source, expected));
  }
  return false;
}

function visibleProperty(symbol) {
  if (symbol.name.startsWith('#')) return false;
  return (symbol.declarations ?? []).some((declaration) => {
    if (!ts.isPropertyDeclaration(declaration) && !ts.isPropertySignature(declaration)) return false;
    const modifiers = ts.getCombinedModifierFlags(declaration);
    return (modifiers & (ts.ModifierFlags.Private | ts.ModifierFlags.Protected)) === 0;
  });
}

function withoutUndefined(type) {
  if (!type.isUnion()) return type;
  const retained = type.types.filter((candidate) => !(candidate.flags & ts.TypeFlags.Undefined));
  return retained.length === 1 ? retained[0] : type;
}

function openJsonPurpose(operationID, path) {
  if (path.includes('input_schema')) return 'json-schema';
  if (
    operationID.startsWith('beta.sessions.')
    && operationID.endsWith('.events.list')
    && path.at(-2) === 'input'
    && path.at(-1) === '*'
  ) return 'tool-input';
  return undefined;
}

function openJsonContract(operationID, path) {
  const purpose = openJsonPurpose(operationID, path);
  assert.ok(
    purpose,
    `${operationID} response contains unreviewed open JSON at $.${path.join('.')}`,
  );
  return Object.freeze({ kind: 'open-json', purpose });
}

function contractForType(checker, input, operationID, path = [], active = new Set()) {
  const type = withoutUndefined(input);
  if (type.flags & (ts.TypeFlags.Any | ts.TypeFlags.Unknown)) {
    return openJsonContract(operationID, path);
  }
  if (type.flags & ts.TypeFlags.Never) return Object.freeze({ kind: 'never' });
  if (type.flags & ts.TypeFlags.Null) return Object.freeze({ kind: 'null' });

  // Preserve finite values before the broader *Like checks below. Generated
  // Stainless response declarations use literal unions as wire discriminators
  // (`type`, `status`, `injection_location`, ...). Collapsing them to their
  // primitive kind would certify a JSON shape that the selected SDK can parse
  // but whose behavior is observably wrong.
  if (type.flags & ts.TypeFlags.StringLiteral) {
    return Object.freeze({ kind: 'literal', primitive: 'string', value: type.value });
  }
  if (type.flags & ts.TypeFlags.NumberLiteral) {
    return Object.freeze({ kind: 'literal', primitive: 'number', value: type.value });
  }
  if (type.flags & ts.TypeFlags.BooleanLiteral) {
    return Object.freeze({
      kind: 'literal',
      primitive: 'boolean',
      value: type.intrinsicName === 'true',
    });
  }

  if (type.isUnion()) {
    const variants = type.types
      .filter((candidate) => !(candidate.flags & ts.TypeFlags.Undefined))
      .map((candidate) => contractForType(checker, candidate, operationID, path, active));
    const unique = new Map(variants.map((variant) => [JSON.stringify(variant), variant]));
    return Object.freeze({ kind: 'union', variants: [...unique.values()] });
  }

  if (type.isIntersection()) {
    if (type.types.some((candidate) => candidate.flags & ts.TypeFlags.StringLike)) {
      return Object.freeze({ kind: 'string' });
    }
    if (type.types.some((candidate) => candidate.flags & ts.TypeFlags.NumberLike)) {
      return Object.freeze({ kind: 'number' });
    }
    if (type.types.some((candidate) => candidate.flags & ts.TypeFlags.BooleanLike)) {
      return Object.freeze({ kind: 'boolean' });
    }
  }

  if (type.flags & ts.TypeFlags.StringLike) return Object.freeze({ kind: 'string' });
  if (type.flags & ts.TypeFlags.NumberLike) return Object.freeze({ kind: 'number' });
  if (type.flags & ts.TypeFlags.BooleanLike) return Object.freeze({ kind: 'boolean' });
  if (type.flags & ts.TypeFlags.BigIntLike) return Object.freeze({ kind: 'number' });

  if (checker.isArrayType(type) || checker.isTupleType(type)) {
    const arguments_ = checker.getTypeArguments(type);
    const item = arguments_.length === 0
      ? openJsonContract(operationID, [...path, '[]'])
      : contractForType(checker, arguments_[0], operationID, [...path, '[]'], active);
    return Object.freeze({ kind: 'array', item });
  }

  const identity = type.id;
  assert.ok(
    !active.has(identity),
    `${operationID} response contains a recursive type at $.${path.join('.')}`,
  );
  active.add(identity);
  try {
    const properties = {};
    for (const property of checker.getPropertiesOfType(type).filter(visibleProperty)) {
      const declaration = property.valueDeclaration ?? property.declarations?.[0];
      assert.ok(declaration, `${property.name}: public property has no declaration`);
      properties[property.name] = Object.freeze({
        required: (property.flags & ts.SymbolFlags.Optional) === 0,
        value: contractForType(
          checker,
          checker.getTypeOfSymbolAtLocation(property, declaration),
          operationID,
          [...path, property.name],
          active,
        ),
      });
    }
    const index = checker.getIndexTypeOfType(type, ts.IndexKind.String);
    return Object.freeze({
      kind: 'object',
      properties: Object.freeze(Object.fromEntries(
        Object.entries(properties).sort(([left], [right]) => left.localeCompare(right)),
      )),
      additional: index
        ? contractForType(checker, index, operationID, [...path, '*'], active)
        : false,
    });
  } finally {
    active.delete(identity);
  }
}

function responseContract(checker, method, source, operationID) {
  assert.ok(method.type, `${method.name.getText(source)}: response type is absent`);
  const outer = method.type;
  const wrapper = referenceName(outer, source);
  assert.ok(
    wrapper === 'APIPromise' || wrapper === 'PagePromise',
    `${method.name.getText(source)}: unsupported response wrapper ${outer.getText(source)}`,
  );
  assert.ok(outer.typeArguments?.length, `${method.name.getText(source)}: response payload is absent`);
  if (wrapper === 'PagePromise') {
    return Object.freeze({
      kind: 'json',
      schema: contractForType(
        checker,
        checker.getTypeFromTypeNode(outer.typeArguments[0]),
        operationID,
      ),
      // A structurally valid empty page says nothing about the element DTO.
      // Keep this generated evidence obligation adjacent to the official
      // PagePromise wrapper so every behavior owner must observe at least one
      // real item without maintaining a second list-operation inventory.
      evidence: Object.freeze({ nonEmptyArrays: Object.freeze([Object.freeze(['data'])]) }),
    });
  }
  const payload = outer.typeArguments[0];
  if (referenceName(payload, source) === 'Response') return Object.freeze({ kind: 'binary' });
  if (containsReference(payload, source, 'Stream')) return Object.freeze({ kind: 'stream' });
  if (payload.kind === ts.SyntaxKind.VoidKeyword) return Object.freeze({ kind: 'empty' });
  return Object.freeze({
    kind: 'json',
    schema: contractForType(checker, checker.getTypeFromTypeNode(payload), operationID),
  });
}

export function extractResponseContractsFromPackageRoot(root, scope, operationIDs) {
  const sdk = sdkPackageFromRoot(root);
  const expected = new Set(operationIDs);
  assert.ok(expected.size > 0, 'response extraction requires operation identities');
  const sources = scopedJavaScriptFiles(root, scope).map((entry) => ({
    ...entry,
    declaration: declarationPath(entry.filename),
  }));
  for (const { declaration } of sources) {
    assert.ok(fs.existsSync(declaration), `${declaration}: SDK declaration is absent`);
  }
  const program = ts.createProgram({
    rootNames: sources.map(({ declaration }) => declaration),
    options: {
      module: ts.ModuleKind.NodeNext,
      moduleResolution: ts.ModuleResolutionKind.NodeNext,
      skipLibCheck: true,
      strictNullChecks: true,
      target: ts.ScriptTarget.ESNext,
    },
  });
  const checker = program.getTypeChecker();
  const contracts = new Map();
  for (const { declaration, filename, prefix, resourceRoot } of sources) {
    const source = program.getSourceFile(declaration);
    assert.ok(source, `${declaration}: declaration was not loaded`);
    const namespace = operationNamespace(resourceRoot, filename, prefix);
    const visit = (node) => {
      if (ts.isMethodDeclaration(node) && node.name && ts.isIdentifier(node.name)) {
        const id = `${namespace}.${node.name.text}`;
        if (expected.has(id)) {
          let contract;
          try {
            contract = responseContract(checker, node, source, id);
          } catch (error) {
            throw new Error(`${sdk.version} ${id}: ${error.message}`, { cause: error });
          }
          const prior = contracts.get(id);
          assert.ok(
            !prior || JSON.stringify(prior) === JSON.stringify(contract),
            `${id}: overloaded responses disagree`,
          );
          contracts.set(id, contract);
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(source);
  }
  assert.deepEqual(
    [...contracts.keys()].sort(),
    [...expected].sort(),
    `${sdk.version}: every official operation has one response contract`,
  );
  return Object.freeze(Object.fromEntries([...contracts].sort(([left], [right]) => left.localeCompare(right))));
}
