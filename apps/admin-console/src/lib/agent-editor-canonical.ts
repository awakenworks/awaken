/**
 * Stable JSON encoding: deep-sorts object keys so structurally-equal values
 * always serialize to the same string. Used as a building block for
 * `deepEqualCanonical` so re-formatting / key-reordering of Raw JSON doesn't
 * read as a locked-field change or trigger a spurious PATCH entry.
 *
 * Arrays preserve order (semantically significant). `undefined` collapses to
 * `null` so `{a: undefined}` and `{}` are treated equivalently. JSON has no
 * `undefined`, and field absence vs explicit-undefined should not surface as
 * a diff in this editor.
 */
export function canonicalStringify(value: unknown): string {
  return JSON.stringify(canonicalize(value));
}

function canonicalize(value: unknown): unknown {
  if (value === undefined || value === null) return null;
  if (Array.isArray(value)) return value.map(canonicalize);
  if (typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>)
      .filter(([, item]) => item !== undefined)
      .map(([key, item]) => [key, canonicalize(item)] as const)
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
    return Object.fromEntries(entries);
  }
  return value;
}

/** Structural equality using `canonicalStringify`: order-insensitive for
 *  object keys, order-sensitive for arrays. */
export function deepEqualCanonical(a: unknown, b: unknown): boolean {
  return canonicalStringify(a) === canonicalStringify(b);
}
