import assert from 'node:assert/strict';
import { test } from 'node:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { parseDurableJson, sqliteDatabaseForThread, sqliteExec, sqliteRun, stringifyDurableJson } from './sqlite.mjs';
import * as memoryExtraction from './fixtures/memory_extraction_fixture.mjs';

test('Run facts locate a unique thread partition before any message is committed', () => {
  // Cause/effect table: L1 an exact Run fact, without message rows, identifies
  // one Runtime partition; L2 no exact Thread fact refuses without creating a
  // database; L3 two exact partitions refuse instead of choosing a first file.
  // Run state is the authority even when inference never committed a message.
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-run-fact-'));
  const database = path.join(directory, 'runtime.db');
  const create = (file) => {
    sqliteExec(file, `CREATE TABLE runtime_run_record (
      run_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, phase TEXT NOT NULL
    )`);
    sqliteRun(file, 'INSERT INTO runtime_run_record VALUES (?, ?, ?)',
      'aux-run', 'aux-thread', '"Awaiting"');
  };
  try {
    create(database);
    assert.equal(sqliteDatabaseForThread(directory, 'aux-thread', 'runtime_run_record'),
      database, 'L1 no message is required to observe a Run fact');
    const files = fs.readdirSync(directory).sort();
    assert.throws(() => sqliteDatabaseForThread(directory, 'missing-thread', 'runtime_run_record'),
      /matches=\[\]/u, 'L2 no guessed partition');
    assert.deepEqual(fs.readdirSync(directory).sort(), files, 'L2 query creates no file');
    create(path.join(directory, 'competing.db'));
    assert.throws(() => sqliteDatabaseForThread(directory, 'aux-thread', 'runtime_run_record'),
      /thread must have one/u, 'L3 competing Run authorities are refused');
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

// Durable-JSON cause/effect graph: C1 an integer token is safe; C2 an integer
// token exceeds Number's exact range; C3 the same digits occur inside a string;
// C4 a numeric token uses decimal/exponent notation; C5 values are nested.
// Effects: E1 ordinary JSON types are unchanged; E2 unsafe integer bytes survive
// a read/edit/write cycle; E3 strings and non-integer notation are not recast.
// Decision rules: C1=>E1; C2=>E2; C3||C4=>E3; any rule+C5=>same effect.
test('round-trips durable unsafe integers without changing other JSON values', () => {
  const source = '{"safe":42,"lease":18446744073709551615,"negative":-9007199254740993,"label":"18446744073709551615","decimal":1.5,"exponent":1e+21,"nested":[18446744073709551614]}';
  const parsed = parseDurableJson(source);

  assert.equal(parsed.safe, 42, 'C1 => E1');
  assert.equal(parsed.lease, 18446744073709551615n, 'C2 => E2');
  assert.equal(parsed.negative, -9007199254740993n, 'negative C2 => E2');
  assert.equal(parsed.label, '18446744073709551615', 'C3 => E3');
  assert.equal(parsed.decimal, 1.5, 'decimal C4 => E3');
  assert.equal(parsed.exponent, 1e21, 'exponent C4 => E3');
  assert.equal(parsed.nested[0], 18446744073709551614n, 'C2+C5 => E2');

  parsed.safe = 43;
  const encoded = stringifyDurableJson(parsed);
  assert.match(encoded, /"safe":43/);
  assert.match(encoded, /"lease":18446744073709551615/);
  assert.match(encoded, /"negative":-9007199254740993/);
  assert.match(encoded, /"label":"18446744073709551615"/);
  assert.match(encoded, /"nested":\[18446744073709551614\]/);
  assert.deepEqual(parseDurableJson(encoded), parsed);
});

// Retained-extraction fixture decision table (real SQLite, no Runtime model):
// | Rule | input cause | observation/effect |
// | X1 | current known frame | exact intent and outer SQL coordinates, no wrapper |
// | X2 | published bare row, no execution | exact legacy value, no defaults |
// | X3 | unsafe u64 in either shape | exact BigInt/raw integer via sqlite.mjs |
// | X4 | unknown/partial/competing frame or bare execution | throw, never fallback |
// | X5 | deliberately invalid aggregate field | frame preserves it for Rust rejection |
// These opaque values test only the fixture's storage-frame projection. The
// complete intent/state/lease validation remains covered by SessionStore's
// extraction codec and the unchanged real crash/stage-recovery scenarios.
test('retained extraction fixture preserves the canonical frame and rejects downgrade', () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-extraction-frame-'));
  const database = path.join(directory, 'retained.db');
  try {
    sqliteExec(database, `CREATE TABLE managed_memory_extraction (
      intent_id TEXT PRIMARY KEY, status TEXT NOT NULL, data TEXT NOT NULL
    )`);
    const intent = {
      intent_id: 'intent-a', auxiliary_run_id: 'run-a', auxiliary_thread_id: 'thread-a',
      status: 'claimed', revision: 2, lease_expires_at_unix_ms: 18446744073709551615n,
      execution: { commit_scope: 'session', data_subject_id: 'subject-a' },
      mutations: [], receipt: null,
    };
    const current = stringifyDurableJson({ format: 'awaken.memory-extraction.v1', extraction: intent });
    sqliteRun(database, 'INSERT INTO managed_memory_extraction VALUES (?, ?, ?)',
      intent.intent_id, intent.status, current);
    const [row] = memoryExtraction.extractionRows(database);
    assert.equal(row.intent.intent_id, intent.intent_id, 'X1 canonical intent coordinate');
    assert.deepEqual(row.intent, intent, 'X1 exact auxiliary identity and execution');
    assert.equal(row.intent_id, intent.intent_id, 'X1 outer SQL identity remains visible');
    assert.equal(row.status, intent.status, 'X1 outer SQL status remains visible');
    assert.equal(row.data, current, 'X1 original bytes remain observable');
    assert.equal(row.intent.lease_expires_at_unix_ms, 18446744073709551615n, 'X3 no u64 rounding');

    const { execution: _execution, ...legacy } = intent;
    sqliteRun(database, 'UPDATE managed_memory_extraction SET data = ?', stringifyDurableJson(legacy));
    assert.deepEqual(memoryExtraction.extractionRows(database)[0].intent, legacy, 'X2 exact legacy');

    const invalidAggregate = { ...intent, memory_config_version: 0 };
    const encoded = memoryExtraction.extractionData(invalidAggregate);
    assert.deepEqual(parseDurableJson(encoded), {
      format: 'awaken.memory-extraction.v1', extraction: invalidAggregate,
    }, 'X5 preserve invalid fields for the real validator');
    assert.match(encoded, /18446744073709551615/u, 'X3 write retains raw u64');
    sqliteRun(database, 'UPDATE managed_memory_extraction SET data = ?', encoded);
    assert.deepEqual(memoryExtraction.extractionRows(database)[0].intent,
      invalidAggregate, 'X5 fixture does not repair aggregate state');

    const rejected = [
      { data: 'not json', error: SyntaxError },
      ...[null, 42, [],
        { format: 'awaken.memory-extraction.v999', extraction: intent },
        { format: 'awaken.memory-extraction.v1' },
        { extraction: intent },
        { format: 'awaken.memory-extraction.v1', extraction: null },
        { format: 'awaken.memory-extraction.v1', extraction: [] },
        { format: 'awaken.memory-extraction.v1', extraction: 42 },
        { format: 'awaken.memory-extraction.v1', extraction: intent, intent_id: 'competing' },
        { ...legacy, execution: null },
        { ...legacy, execution: { commit_scope: 'auxiliary_thread' } },
      ].map((value) => ({ data: stringifyDurableJson(value), error: TypeError })),
    ];
    for (const [index, { data, error }] of rejected.entries()) {
      sqliteRun(database, 'UPDATE managed_memory_extraction SET data = ?', data);
      assert.throws(() => memoryExtraction.extractionRows(database), error,
        `X4 reject frame ${index} instead of silently dropping identity/context`);
    }
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});
