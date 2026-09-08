import assert from 'node:assert/strict';
import {test} from 'node:test';
import {postgresQuery,postgresRows} from './postgres.mjs';

const connection = {container:process.env.AWAKEN_E2E_POSTGRES_CONTAINER,
  url:process.env.SESSION_DEPLOYMENT_DATABASE_URL,
  database:process.env.SESSION_DEPLOYMENT_DATABASE_URL
    ? new URL(process.env.SESSION_DEPLOYMENT_DATABASE_URL).pathname.slice(1) : undefined};

test('Postgres observation preserves rows, quoted identity, bytes, and read-only authority',
  {skip:!connection.url}, () => {
  // P1 real row, quoted ID, arbitrary bytes => exact values; P2 zero rows => [];
  // P3 missing/extra argument => fail before execution; P4 attempted write
  // through observation => database rejects and existing bytes remain intact.
  // Only this test's fresh table is mutated through the explicit injection edge.
  const table = `probe_${process.pid}`;
  try {
    postgresQuery(connection,`CREATE TABLE ${table}(id text, bytes bytea);
`+
      `INSERT INTO ${table} VALUES ('quote''id', decode('00ff0a','hex'));`);
    assert.deepEqual(postgresRows(connection,`SELECT id,encode(bytes,'base64') AS bytes FROM ${table} WHERE id=?`,"quote'id"),
      [{id:"quote'id",bytes:'AP8K'}]);
    assert.deepEqual(postgresRows(connection,`SELECT id FROM ${table} WHERE id=?`,'missing'),[]);
    assert.throws(()=>postgresRows(connection,'SELECT ?'),/missing/);
    assert.throws(()=>postgresRows(connection,'SELECT 1','extra'),/extra/);
    assert.throws(()=>postgresRows(connection,
      `SELECT 1) observation; DELETE FROM ${table}; SELECT row_to_json(observation) FROM (SELECT 1`),
      /PostgreSQL test query failed/);
    assert.deepEqual(postgresRows(connection,`SELECT encode(bytes,'hex') AS bytes FROM ${table}`),[{bytes:'00ff0a'}]);
  } finally { postgresQuery(connection,`DROP TABLE IF EXISTS ${table}`); }
});
