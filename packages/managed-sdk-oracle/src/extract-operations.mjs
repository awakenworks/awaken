import fs from 'node:fs';
import path from 'node:path';
import ts from 'typescript';

import { normalizeRoute, operationNamespace } from './normalize.mjs';
import { resolveSdkPackage, sdkPackageFromRoot, walkJavaScript } from './package-source.mjs';

const HTTP_METHODS = new Map([
  ['delete', 'DELETE'],
  ['get', 'GET'],
  ['getAPIList', 'GET'],
  ['patch', 'PATCH'],
  ['post', 'POST'],
  ['put', 'PUT'],
]);
const BETA_PATTERN = /^[a-z][a-z0-9-]*-\d{4}-\d{2}-\d{2}$/;

function routeText(node, source) {
  if (ts.isStringLiteralLike(node)) return node.text;
  if (ts.isTaggedTemplateExpression(node)) {
    const template = node.template;
    if (ts.isNoSubstitutionTemplateLiteral(template)) return template.text;
    let value = template.head.text;
    for (const span of template.templateSpans) value += '${parameter}' + span.literal.text;
    return value;
  }
  throw new Error(`Unsupported SDK route expression: ${node.getText(source)}`);
}

function methodName(node) {
  if (!node.name) return undefined;
  if (ts.isIdentifier(node.name) || ts.isStringLiteralLike(node.name)) return node.name.text;
  return undefined;
}

function betaTokens(node) {
  const tokens = new Set();
  const visit = (candidate) => {
    if (ts.isStringLiteralLike(candidate) && BETA_PATTERN.test(candidate.text)) {
      tokens.add(candidate.text);
    }
    ts.forEachChild(candidate, visit);
  };
  visit(node);
  return [...tokens].sort();
}

function operationsInFile(resourceRoot, filename, prefix) {
  const text = fs.readFileSync(filename, 'utf8');
  const source = ts.createSourceFile(filename, text, ts.ScriptTarget.Latest, true);
  const namespace = operationNamespace(resourceRoot, filename, prefix);
  const operations = [];
  const visit = (node) => {
    if (ts.isMethodDeclaration(node) && node.body) {
      const operation = methodName(node);
      if (!operation) return ts.forEachChild(node, visit);
      const calls = [];
      const findCall = (candidate) => {
        if (
          ts.isCallExpression(candidate)
          && ts.isPropertyAccessExpression(candidate.expression)
          && HTTP_METHODS.has(candidate.expression.name.text)
          && candidate.arguments.length > 0
        ) {
          const receiver = candidate.expression.expression.getText(source);
          if (receiver.endsWith('._client') || receiver === 'this._client') calls.push(candidate);
        }
        ts.forEachChild(candidate, findCall);
      };
      findCall(node.body);
      if (calls.length > 1) {
        throw new Error(`${filename}:${operation} contains more than one HTTP operation`);
      }
      if (calls.length === 1) {
        const call = calls[0];
        const route = routeText(call.arguments[0], source);
        const transportQuery = route.split('?', 2)[1];
        operations.push({
          id: `${namespace}.${operation}`,
          method: HTTP_METHODS.get(call.expression.name.text),
          path: normalizeRoute(route),
          ...(transportQuery ? { transport_query: transportQuery } : {}),
          betas: betaTokens(node.body),
        });
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  return operations;
}

export function extractOperations(moduleName, scope) {
  const sdk = resolveSdkPackage(moduleName);
  return extractOperationsFromPackageRoot(sdk.root, scope, moduleName);
}

export function extractOperationsFromPackageRoot(root, scope, moduleName = '@anthropic-ai/sdk') {
  const sdk = sdkPackageFromRoot(root);
  const betaRoot = path.join(root, 'resources', 'beta');
  const betaAllowed = new Set(scope.beta_resource_roots);
  const betaFiles = walkJavaScript(betaRoot).filter((filename) => {
    const root = path.relative(betaRoot, filename).split(path.sep)[0].replace(/\.js$/, '');
    return betaAllowed.has(root);
  });
  const gaRoot = path.join(root, 'resources');
  const gaAllowed = new Set(scope.ga_resource_roots ?? []);
  const gaFiles = walkJavaScript(gaRoot).filter((filename) => {
    const root = path.relative(gaRoot, filename).split(path.sep)[0].replace(/\.js$/, '');
    return gaAllowed.has(root);
  });
  const byId = new Map();
  const operations = [
    ...betaFiles.flatMap((filename) => operationsInFile(betaRoot, filename, 'beta')),
    ...gaFiles.flatMap((filename) => operationsInFile(gaRoot, filename, '')),
  ];
  for (const operation of operations) {
    const previous = byId.get(operation.id);
    if (previous && JSON.stringify(previous) !== JSON.stringify(operation)) {
      throw new Error(`Conflicting official operation ${operation.id}`);
    }
    byId.set(operation.id, operation);
  }
  return {
    module: moduleName,
    version: sdk.version,
    operations: [...byId.values()].sort((left, right) => left.id.localeCompare(right.id)),
  };
}
