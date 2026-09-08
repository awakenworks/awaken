import assert from 'node:assert/strict';
import { sqliteRows } from '../sqlite.mjs';

// Assertion for the runner's fresh, single-ACP self-managed installation, NOT
// an inventory service or a general restriction on legitimate managed Vaults.
// Query metadata and secret COUNT in one SQLite snapshot; never select sealed
// bytes, open a vendor login file, or print a malformed credential envelope.
export function assertWorkerLocalCredentialCustody(database, runtime, previous) {
  const rows = sqliteRows(database, `
    SELECT data, (SELECT COUNT(*) FROM credential_secret) AS secret_count
    FROM credential_source
  `);
  assert.equal(rows.length, 1, 'local-login installation retains exactly one source');
  assert.equal(rows[0].secret_count, 0, 'local login never enters the sealed SecretStore');
  let source;
  try { source = JSON.parse(rows[0].data); }
  catch { throw new Error('local-login credential metadata is not valid JSON'); }
  assert.ok(source && !Array.isArray(source) && typeof source === 'object',
    'local-login credential metadata is an object');
  assert.ok(source.kind === 'worker_local' && source.status === 'active'
    && source.worker_local_binding?.driver_id === `acp:${runtime}`
    && source.worker_local_binding?.subject_id === 'default',
  'local login retains the selected active WorkerLocal binding');
  assert.ok(source.material_ref == null && source.env_key == null && source.oauth_command == null
    && (source.auxiliary_material_refs === undefined || (
      source.auxiliary_material_refs !== null && typeof source.auxiliary_material_refs === 'object'
      && !Array.isArray(source.auxiliary_material_refs)
      && Object.keys(source.auxiliary_material_refs).length === 0)),
  'WorkerLocal source contains no material or environment/helper reference');
  assert.ok(typeof source.id === 'string' && source.id.length > 0
    && typeof source.workspace_id === 'string' && source.workspace_id.length > 0
    && Number.isSafeInteger(source.version) && source.version > 0,
  'WorkerLocal source has an exact durable identity and revision');
  const snapshot = Object.freeze({ id: source.id, workspace: source.workspace_id, revision: source.version });
  if (previous) assert.ok(snapshot.id === previous.id && snapshot.workspace === previous.workspace
    && snapshot.revision === previous.revision,
  'restart/resource work preserves the same WorkerLocal identity and revision');
  return snapshot;
}
