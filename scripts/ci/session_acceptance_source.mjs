// Read source as syntax only: an acceptance check never imports or executes E2E.
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import ts from 'typescript';

const text = readFileSync(0, 'utf8');
const source = ts.createSourceFile('acceptance.mjs', text, ts.ScriptTarget.Latest, true, ts.ScriptKind.JS);
if (source.parseDiagnostics.length) {
  throw new Error(ts.flattenDiagnosticMessageText(source.parseDiagnostics[0].messageText, '\n'));
}
const digest = value => createHash('sha256').update(JSON.stringify(value)).digest('hex');
function syntax(node) {
  const children = [];
  ts.forEachChild(node, child => { children.push(syntax(child)); });
  return [ts.SyntaxKind[node.kind], node.text ?? null, children];
}
function functionName(node) {
  if (!ts.isFunctionLike(node)) return undefined;
  return node.name?.text ?? (ts.isVariableDeclaration(node.parent) ? node.parent.name.getText(source) : undefined);
}
function owner(node) {
  const names = [];
  for (let current = node; current; current = current.parent) {
    const name = functionName(current);
    if (name) names.unshift(name);
  }
  return names.join('.');
}
function position(node) {
  return {
    line: source.getLineAndCharacterOfPosition(node.getStart(source)).line + 1,
    end_line: source.getLineAndCharacterOfPosition(node.end).line + 1,
  };
}
function literals(node) {
  const values = [];
  function visit(current) {
    if (ts.isStringLiteralLike(current) || ts.isTemplateHead(current)
        || ts.isTemplateMiddle(current) || ts.isTemplateTail(current)) values.push(current.text);
    ts.forEachChild(current, visit);
  }
  visit(node);
  return values;
}
const result = { functions: [], assertions: [], statements: [] };
function visit(node) {
  if (functionName(node)) result.functions.push({ function: owner(node), ...position(node) });
  if (ts.isExpressionStatement(node)) {
    result.statements.push({ statement_sha256: digest(syntax(node)), literals: literals(node), ...position(node) });
  }
  if (ts.isCallExpression(node)) {
    const callee = node.expression.getText(source).replace(/\s+/gu, '');
    if (/^(?:assert(?:\.\w+)?|expect)$/u.test(callee)) {
      result.assertions.push({ function: owner(node) || '<module>',
        assertion_sha256: digest(syntax(node)), ...position(node) });
    }
  }
  ts.forEachChild(node, visit);
}
visit(source);
process.stdout.write(JSON.stringify(result));
