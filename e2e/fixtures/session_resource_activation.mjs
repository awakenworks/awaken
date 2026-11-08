// One retained-row crash seam for Session Resource generation E2Es. The Rust
// SessionResourceState::prepare and Session Store codec own transition validity;
// this fixture encodes their post-Prepared/pre-effect window for an unchanged
// manifest. SQLite admission and exact durable integers stay in sqlite.mjs.
import assert from 'node:assert/strict';
import { parseDurableJson, sqliteRows, sqliteRun, stringifyDurableJson } from '../sqlite.mjs';

export function sessionResourceEnvelope(database, sessionId) {
  const rows = sqliteRows(database,
    'SELECT aggregate_json FROM managed_session WHERE session_id = ?', sessionId);
  assert.equal(rows.length, 1, `missing durable Session ${sessionId}`);
  assert.ok(rows[0].aggregate_json, `Session ${sessionId} has no canonical aggregate`);
  const envelope = parseDurableJson(rows[0].aggregate_json);
  assert.equal(envelope.format, 'awaken.session.v2');
  return envelope;
}

export function persistPreparedGeneration(database, sessionId, { lastError } = {}) {
  assert.ok(lastError === undefined || typeof lastError === 'string', 'prepared diagnostic must be text');
  const envelope = sessionResourceEnvelope(database, sessionId);
  const aggregate = envelope.aggregate;
  const state = aggregate.resources;
  assert.equal(aggregate.status, 'idle', 'crash seam requires a settled baseline Run');
  assert.equal(state.pending, undefined, 'crash seam requires no pending generation');
  assert.ok(typeof state.revision === 'bigint' || Number.isSafeInteger(state.revision));
  const previousRevision = BigInt(state.revision);
  assert.ok(previousRevision >= 0n && previousRevision < (1n << 64n) - 1n,
    'crash seam requires an incrementable Resource revision');
  const revision = previousRevision + 1n;
  const previous = state.activations.map((activation) => ({
    ...activation,
    state: activation.state === 'active' ? 'releasing' : activation.state,
  }));
  const prepared = state.activations.filter((activation) => activation.state === 'active')
    .map((activation) => ({
      activation_id: `${sessionId}:${revision}:${activation.binding_id}`,
      session_id: activation.session_id,
      revision,
      binding_id: activation.binding_id,
      resource_id: activation.resource_id,
      access: activation.access,
      state: 'prepared',
      attempts: 0,
      ...(lastError === undefined ? {} : { last_error: lastError }),
    }));
  assert.ok(prepared.length > 0, 'crash seam requires an Active generation');
  // Preserve the installed revision, retirement intents and every unrelated
  // aggregate field. New activation records inherit identity, never the old
  // generation's physical lease or diagnostics (prepared_activation in Rust).
  const resources = { ...state, revision, pending: state.active, activations: [...previous, ...prepared] };
  sqliteRun(database,
    'UPDATE managed_session SET aggregate_json = ? WHERE session_id = ?',
    stringifyDurableJson({ ...envelope, aggregate: { ...aggregate, resources } }), sessionId);
}
