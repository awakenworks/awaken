import { sqliteRows } from '../sqlite.mjs';

// One retained-intent observation for durable, crash and stage-recovery E2Es.
// SQLite recovery/query-only admission stays owned by sqlite.mjs; malformed
// persisted payloads are errors, never an empty or apparently completed queue.
export function extractionRows(database) {
  return sqliteRows(database,
    'SELECT intent_id, status, data FROM managed_memory_extraction ORDER BY intent_id',
  ).map((row) => ({ ...row, intent: JSON.parse(row.data) }));
}
