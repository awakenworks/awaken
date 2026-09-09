import test from 'node:test';
import assert from 'node:assert/strict';
import {DatabaseSync} from 'node:sqlite';
import {CURRENT_WORKER_ROWS_QUERY} from './standalone_host_worker.mjs';

test('registration observation selects current generations without erasing history', () => {
  // R1 old Ready + newer Starting/Ready/Dead => only newer state, never fallback.
  // R2 independent Worker IDs + reversed insertion order => one maximum per ID.
  // R3 reading must preserve every historical row; PostgreSQL uses this exact
  // query in the Cowork crash/reopen journey, not a separate selector.
  for (const state of ['starting','ready','dead']) for (const reverse of [false,true]) {
    const db=new DatabaseSync(':memory:');
    try {
      db.exec(`CREATE TABLE worker_registry_worker(worker_id TEXT,incarnation_id TEXT,
        generation INTEGER,state TEXT,expires_at_ms INTEGER,manifest_json TEXT,
        credential_observations_json TEXT,observation_sequence INTEGER)`);
      const rows=[['a','old',1,'ready'],['a','new',2,state],['b','other',1,'ready']];
      for (const row of reverse ? rows.toReversed() : rows) {
        db.prepare('INSERT INTO worker_registry_worker VALUES(?,?,?,?,0,?, ?,0)').run(...row,'{}','[]');
      }
      assert.deepEqual(db.prepare(CURRENT_WORKER_ROWS_QUERY).all().map(r=>[r.worker_id,r.incarnation_id,r.generation,r.state]),
        [['a','new',2,state],['b','other',1,'ready']]);
      assert.equal(db.prepare('SELECT COUNT(*) AS count FROM worker_registry_worker').get().count,3);
    } finally {db.close();}
  }
});
