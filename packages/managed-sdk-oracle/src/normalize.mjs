import path from 'node:path';

export function normalizeRoute(value) {
  const withoutQuery = value.split('?', 1)[0];
  return withoutQuery
    .replaceAll(/\$\{[^}]+\}/g, '{}')
    .replaceAll(/\{\*[^}]*\}/g, '{}')
    .replaceAll(/\{[^}]*\}/g, '{}');
}

function camel(value) {
  return value.replaceAll(/-([a-z])/g, (_, letter) => letter.toUpperCase());
}

export function operationNamespace(betaRoot, filename) {
  const relative = path.relative(betaRoot, filename).replaceAll(path.sep, '/');
  const segments = relative.replace(/\.js$/, '').split('/');
  if (segments.length > 1 && segments.at(-1) === segments.at(-2)) segments.pop();
  if (segments.at(-1) === 'index') segments.pop();
  return ['beta', ...segments.map(camel)].join('.');
}

export function stableJson(value) {
  if (Array.isArray(value)) return value.map(stableJson);
  if (value && typeof value === 'object') {
    return Object.fromEntries(
      Object.entries(value)
        .sort(([left], [right]) => left.localeCompare(right))
        .map(([key, nested]) => [key, stableJson(nested)]),
    );
  }
  return value;
}
