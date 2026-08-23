// Semantic normalizer for Managed Agents wire events.
//
// Strips the volatile fields that legitimately vary run-to-run (minted ids, RFC
// 3339 timestamps, id cross-references) so two captures of the same logical flow
// compare equal. Shared by the golden gate and any e2e that wants a stable diff
// instead of hand-picking fields. Pure and dependency-free.

// Keys whose values are non-deterministic bookkeeping, dropped wholesale.
const VOLATILE_KEYS = new Set([
  'id',
  'processed_at',
  'created_at',
  'updated_at',
  'archived_at',
  'next_page',
]);

// Minted-id value shapes (evt_7, sesn_3, sthr_1, call-9, msg_2, and the
// canonical magent_v1_* projection ids) → a placeholder, so id
// cross-references (e.g. model_request_start_id or stop_reason.event_ids)
// normalize too. Keep this vocabulary exact: arbitrary external strings that
// happen to end in `_id` are semantic payload, not volatile bookkeeping.
const ID_VALUE = /^(?:(?:evt|sesn|sthr|msg|call)[-_]|magent_v1_).+$/;

export function normalize(value) {
  if (Array.isArray(value)) return value.map(normalize);
  if (value && typeof value === 'object') {
    const out = {};
    for (const [k, v] of Object.entries(value)) {
      if (VOLATILE_KEYS.has(k)) continue;
      out[k] = normalize(v);
    }
    return out;
  }
  if (typeof value === 'string' && ID_VALUE.test(value)) return '<id>';
  return value;
}

export const normalizeEvents = (events) => events.map(normalize);
