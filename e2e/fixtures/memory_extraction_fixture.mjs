import { parseDurableJson, sqliteRows, stringifyDurableJson } from '../sqlite.mjs';

// This is the existing retained-row fixture boundary, not another aggregate
// validator. SessionStore's Rust codec owns intent validity and recovery. Keep
// its published frame here; SQLite admission and lossless JSON stay in sqlite.mjs.
const EXTRACTION_FORMAT = 'awaken.memory-extraction.v1';

function extractionValue(data) {
  const value = parseDurableJson(data);
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError('Memory extraction fixture requires an object frame');
  }
  if (Object.hasOwn(value, 'format') || Object.hasOwn(value, 'extraction')) {
    if (value.format !== EXTRACTION_FORMAT || Object.keys(value).length !== 2
      || value.extraction === null || typeof value.extraction !== 'object'
      || Array.isArray(value.extraction)) {
      throw new TypeError('Memory extraction fixture requires the complete known frame');
    }
    return value.extraction;
  }
  if (Object.hasOwn(value, 'execution')) {
    throw new TypeError('Memory extraction execution context requires the known frame');
  }
  return value; // Published legacy bare intent, without a new execution context.
}

// Fault injection preserves every supplied aggregate field, including invalid
// values whose rejection belongs to the real repository. Do not validate,
// default or repair the aggregate while selecting its canonical storage frame.
export function extractionData(intent) {
  return stringifyDurableJson({ format: EXTRACTION_FORMAT, extraction: intent });
}

// One retained-intent observation for durable, crash and stage-recovery E2Es.
// Invalid JSON/frames throw; no row becomes an empty or apparently completed
// queue, and no execution context is silently read as an older bare intent.
export function extractionRows(database) {
  return sqliteRows(database,
    'SELECT intent_id, status, data FROM managed_memory_extraction ORDER BY intent_id',
  ).map((row) => ({ ...row, intent: extractionValue(row.data) }));
}
