/** Shared id validation rules for eval resources (datasets, fixtures).
 *
 *  Centralised here so both `CreateDatasetModal` (on /datasets) and
 *  `SaveTraceAsFixtureModal` (`+ Fixture` drawer button) reject the
 *  same way — earlier drafts had per-modal patterns that diverged
 *  (one accepted slashes, another didn't), pushing errors to the
 *  backend's much terser response. The pattern intentionally matches
 *  what the server's `ConfigStore` accepts as a URL slug. */

/** Allowed chars: ASCII alnum + `_ . - `, must start with alnum. */
export const DATASET_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9_.-]*$/;
export const DATASET_ID_PATTERN_SOURCE = DATASET_ID_PATTERN.source;
export const DATASET_ID_MAX_LEN = 128;

export type DatasetIdValidation =
  | { ok: true; value: string }
  | { ok: false; reasonKey: "empty" | "tooLong" | "format" };

export function validateDatasetId(raw: string): DatasetIdValidation {
  const value = raw.trim();
  if (!value) return { ok: false, reasonKey: "empty" };
  if (value.length > DATASET_ID_MAX_LEN) {
    return { ok: false, reasonKey: "tooLong" };
  }
  if (!DATASET_ID_PATTERN.test(value)) {
    return { ok: false, reasonKey: "format" };
  }
  return { ok: true, value };
}
