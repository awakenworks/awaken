import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { sqliteExec, sqliteRun, sqliteRows, stringifyDurableJson } from '../sqlite.mjs';
import { persistPreparedGeneration, sessionResourceEnvelope } from './session_resource_activation.mjs';

function fixture(t) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'resource-prepared-fixture-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const database = path.join(directory, 'sessions.db');
  // Minimal SQL read/write port seam, not another Session Store migration or
  // aggregate validator. Real scenario recovery enters the Rust codec/kernel.
  sqliteExec(database, 'CREATE TABLE managed_session (session_id TEXT PRIMARY KEY, aggregate_json TEXT NOT NULL)');
  return database;
}

function seed(sessionId) {
  return { format: 'awaken.session.v2', aggregate: {
    status: 'idle', untouched: { lease_epoch: 18446744073709551615n },
    resources: { revision: 1, committed_revision: 1,
      active: { inputs: [{ binding_id: 'memory-binding' }] },
      repository_retirements: [{ binding_id: 'retained-repository' }],
      activations: [{ activation_id: 'old-activation', session_id: sessionId,
        revision: 1, binding_id: 'memory-binding',
        resource_id: { kind: 'memory_store', id: 'memory' }, access: 'read_write',
        state: 'active', attempts: 1, lease_expires_at_unix_ms: 42, last_error: 'previous diagnostic',
      }],
    },
  } };
}

function insert(database, sessionId, envelope) {
  sqliteRun(database, 'INSERT INTO managed_session VALUES (?, ?)', sessionId, stringifyDurableJson(envelope));
}

test('one prepared crash seam retains old authority and encodes both diagnostic inputs', (t) => {
  // Causes: C1 Idle/Active current v2 seed; C2 optional pre-effect diagnostic;
  // C3 unrelated committed revision, retirement intent and exact u64 fields.
  // R1 C1+!C2+C3 -> same active inputs/old Releasing record, new Prepared
  // generation with zero attempts and no inherited lease/error. R2 C1+C2+C3
  // -> same result with exactly C2's diagnostic. Both preserve every C3 field;
  // only real Rust reconciliation may promote pending or grant physical work.
  for (const lastError of [undefined, 'process died before realization']) {
    const database = fixture(t);
    const sessionId = "session'quoted";
    const original = seed(sessionId);
    insert(database, sessionId, original);
    persistPreparedGeneration(database, sessionId, { lastError });
    const { aggregate } = sessionResourceEnvelope(database, sessionId);
    assert.equal(aggregate.status, 'idle');
    assert.deepEqual(aggregate.untouched, original.aggregate.untouched);
    const resources = aggregate.resources;
    assert.equal(resources.revision, 2);
    assert.equal(resources.committed_revision, 1);
    assert.deepEqual(resources.repository_retirements, original.aggregate.resources.repository_retirements);
    assert.deepEqual(resources.active, original.aggregate.resources.active);
    assert.deepEqual(resources.pending, original.aggregate.resources.active);
    assert.deepEqual(resources.activations[0], { ...original.aggregate.resources.activations[0], state: 'releasing' });
    assert.deepEqual(resources.activations[1], {
      activation_id: `${sessionId}:2:memory-binding`, session_id: sessionId,
      revision: 2, binding_id: 'memory-binding', resource_id: { kind: 'memory_store', id: 'memory' },
      access: 'read_write', state: 'prepared', attempts: 0,
      ...(lastError === undefined ? {} : { last_error: lastError }),
    });
  }
});

test('generation identities keep exact u64 coordinates beyond JavaScript safe integers', (t) => {
  // R4 a healthy current u64 generation above 2^53 -> the next exact u64
  // revision and activation identity, with the committed predecessor intact.
  // A numeric round-trip or float addition cannot satisfy these fixed values.
  const database = fixture(t);
  const original = seed('large');
  original.aggregate.resources.revision = 9007199254740992n;
  original.aggregate.resources.committed_revision = 9007199254740992n;
  original.aggregate.resources.activations[0].revision = 9007199254740992n;
  insert(database, 'large', original);
  persistPreparedGeneration(database, 'large');
  const resources = sessionResourceEnvelope(database, 'large').aggregate.resources;
  assert.equal(resources.revision, 9007199254740993n);
  assert.equal(resources.committed_revision, 9007199254740992n);
  assert.equal(resources.activations[1].revision, 9007199254740993n);
  assert.equal(resources.activations[1].activation_id, 'large:9007199254740993:memory-binding');
});

test('an unestablished crash precondition refuses without rewriting the retained row', (t) => {
  // C1 wrong frame, C2 non-Idle baseline, C3 pending generation, C4 no Active
  // input, C5 exhausted generation, C6 missing row or non-text diagnostic.
  // Each negates a healthy post-Prepared seam prerequisite:
  // R3=C1|C2|C3|C4|C5|C6 -> fail before SQL UPDATE; original bytes
  // stay intact. The fixture never repairs malformed production aggregates.
  const absent = fixture(t);
  assert.throws(() => persistPreparedGeneration(absent, 'missing'));
  assert.equal(sqliteRows(absent, 'SELECT * FROM managed_session').length, 0);
  const badDiagnostic = fixture(t);
  const valid = seed('session');
  insert(badDiagnostic, 'session', valid);
  assert.throws(() => persistPreparedGeneration(badDiagnostic, 'session', { lastError: {} }));
  assert.equal(sqliteRows(badDiagnostic, 'SELECT aggregate_json FROM managed_session')[0].aggregate_json,
    stringifyDurableJson(valid));
  const mutations = [
    (row) => { row.format = 'awaken.session.v1'; },
    (row) => { row.aggregate.status = 'running'; },
    (row) => { row.aggregate.resources.pending = { inputs: [] }; },
    (row) => { row.aggregate.resources.activations = []; },
    (row) => { row.aggregate.resources.revision = 18446744073709551615n; },
  ];
  for (const mutate of mutations) {
    const database = fixture(t);
    const original = seed('session');
    mutate(original);
    const bytes = stringifyDurableJson(original);
    insert(database, 'session', original);
    assert.throws(() => persistPreparedGeneration(database, 'session'));
    assert.equal(sqliteRows(database, 'SELECT aggregate_json FROM managed_session')[0].aggregate_json, bytes);
  }
});
