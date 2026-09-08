import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { sqliteExec, sqliteRun } from '../sqlite.mjs';
import { assertWorkerLocalCredentialCustody } from './worker_local_credentials.mjs';

function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'worker-local-custody-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const database = path.join(root, 'credential.db');
  // Minimal SQL read-port fixture, not a replacement migration schema. The
  // production CLI resource gate tests these queries against real migrations.
  sqliteExec(database, 'CREATE TABLE credential_source (data TEXT NOT NULL); CREATE TABLE credential_secret (sealed BLOB NOT NULL);');
  const write = (sources, secrets = 0) => {
    sqliteExec(database, 'DELETE FROM credential_source; DELETE FROM credential_secret;');
    for (const source of sources) sqliteRun(database, 'INSERT INTO credential_source VALUES (?)',
      typeof source === 'string' ? source : JSON.stringify(source));
    for (let i = 0; i < secrets; i++) sqliteRun(database, 'INSERT INTO credential_secret VALUES (?)',
      Buffer.from('custody-test-canary'));
  };
  return { database, write };
}

function source(runtime) {
  return { id: 'worker-source', workspace_id: 'workspace', version: 1,
    kind: 'worker_local', status: 'active',
    worker_local_binding: { driver_id: `acp:${runtime}`, subject_id: 'default' } };
}

test('local-login custody survives repeated snapshots without accepting another identity', (t) => {
  // C1 Codex/Claude; C2 omitted/null/empty optional references; C3 first/repeated
  // snapshot; C4 id/workspace/revision changed. R1 C1+C2+!C4 => same frozen
  // secret-free identity; R2 C4 => reject, not silently refresh the baseline;
  // R2 followed by R1 must still accept the original identity.
  const { database, write } = fixture(t);
  for (const runtime of ['codex', 'claude']) {
    const original = source(runtime);
    for (const optional of [{}, { material_ref: null, env_key: null, oauth_command: null,
      auxiliary_material_refs: {} }]) {
      write([{ ...original, ...optional }]);
      const baseline = assertWorkerLocalCredentialCustody(database, runtime);
      assert.ok(Object.isFrozen(baseline));
      for (let restart = 0; restart < 3; restart++) {
        assert.deepEqual(assertWorkerLocalCredentialCustody(database, runtime, baseline), baseline);
      }
      for (const changed of [{ id: 'another' }, { workspace_id: 'another' }, { version: 2 }]) {
        write([{ ...original, ...changed }]);
        assert.throws(() => assertWorkerLocalCredentialCustody(database, runtime, baseline), /preserves the same/);
        write([{ ...original, ...optional }]);
        assert.deepEqual(assertWorkerLocalCredentialCustody(database, runtime, baseline), baseline);
      }
    }
  }
});

test('custody oracle rejects missing, duplicate, wrong or material-bearing sources without leaking them', (t) => {
  // Cause/effect table: R3 row count 0/2 -> reject; R4 any sealed row -> reject;
  // R5 kind/status/driver/subject mismatch -> reject; R6 material/env/OAuth/aux
  // reference (including empty-but-present strings) -> reject; R7 malformed
  // JSON/object/identity/revision -> reject. Cross both real adapter names.
  // Every error must omit the canary, including JSON parser diagnostics.
  const { database, write } = fixture(t);
  const canary = 'custody-test-canary';
  for (const runtime of ['codex', 'claude']) {
    const original = source(runtime);
    const cases = [
      [[], 0], [[original, original], 0], [[original], 1],
      ...[{ kind: 'vault' }, { status: 'disabled' },
        { worker_local_binding: { driver_id: 'acp:another', subject_id: 'default' } },
        { worker_local_binding: { driver_id: `acp:${runtime}`, subject_id: 'another' } },
        { worker_local_binding: null }, { material_ref: canary }, { material_ref: '' },
        { env_key: canary }, { env_key: '' },
        { oauth_command: [canary] }, { oauth_command: [] },
        { auxiliary_material_refs: { token: canary } }, { auxiliary_material_refs: [] },
        { auxiliary_material_refs: null }, { auxiliary_material_refs: '' },
        { id: '' }, { workspace_id: '' }, { version: 0 }, { version: 1.5 },
        { version: Number.MAX_SAFE_INTEGER + 1 },
      ].map((change) => [[{ ...original, ...change }], 0]),
      [[`{"${canary}":`], 0], [['null'], 0], [[[]], 0], [['false'], 0], [['1'], 0],
    ];
    for (const [rows, secrets] of cases) {
      write(rows, secrets);
      assert.throws(() => assertWorkerLocalCredentialCustody(database, runtime), (error) => {
        assert.ok(!String(error).includes(canary), 'failure diagnostics never print credential metadata');
        return true;
      });
    }
  }
  // R8 missing store fails closed without creating a misleading empty DB.
  const missing = path.join(path.dirname(database), 'missing.db');
  assert.throws(() => assertWorkerLocalCredentialCustody(missing, 'codex'), /does not exist/);
  assert.ok(!fs.existsSync(missing));
});
