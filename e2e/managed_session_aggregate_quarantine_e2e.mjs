// Canonical Session aggregate quarantine through real process boundaries.
//
// Cause/effect decision table:
// | Rule | canonical aggregate | missing aggregate | restart | poisoned indexes | Effect |
// | Q1   | matching revision   | no                | yes     | no               | retrieve/update healthy Session |
// | Q2   | yes                 | co-resident row   | yes     | no               | quarantine only corrupt row; exact GET is 500 |
// | Q3   | yes                 | quarantined row   | again   | no               | preserve one quarantine; fabricate no aggregate |
// | Q4   | yes                 | quarantined row   | again   | yes              | canonical aggregate wins over SQL projections |
//
// Effects: E1 healthy work remains available; E2 corrupt durable truth is
// exposed, isolated, and never reconstructed from indexed SQL columns; E3 the
// quarantine is idempotent across restart; E4 canonical title/model/metadata
// survive poisoned projections. Constraints: K1 aggregate_json is the sole
// Session model; K2 SQL columns are indexes/projections, never migration input;
// K3 this fixture only mutates its disposable offline SQLite store and observes
// behavior through the official SDK plus the repository's quarantine evidence.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  deploymentEnv,
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
  waitForValue,
} from './harness.mjs';
import { sqliteExec, sqliteRows } from './sqlite.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38671);
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const CORRUPT_SESSION_ID = 'sesn_missing_aggregate';

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function sqlite(database, sql) {
  return sqliteExec(database, sql);
}

function sqliteJson(database, sql) {
  return sqliteRows(database, sql);
}

function insertMissingAggregateSession(database, scopeId) {
  // Q2/C2: all projection columns are deliberately plausible. The missing
  // canonical aggregate alone is sufficient corruption, so a decoder that
  // reconstructs from these values would violate the one-model boundary.
  sqlite(database, `INSERT INTO managed_session (
      session_id, agent_id, model, title, metadata_json, environment_id,
      mcp_json, scope_id, status, archived_at, effective_inputs_json,
      environment_binding, runtime_json, revision, aggregate_json
    ) VALUES (
      ${sqlQuote(CORRUPT_SESSION_ID)}, 'coder', 'management', 'Indexed title', '{}',
      'env_local', '[]', ${sqlQuote(scopeId)}, 'idle', NULL, '{}', NULL, '{}', 3, NULL
    );`);
}

function client() {
  return new Anthropic({
    apiKey: 'e2e-dummy',
    baseURL: `http://127.0.0.1:${PORT}`,
  });
}

async function main() {
  const management = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-session-aggregate-quarantine-'));
  const upstream = await startUpstream('mcp');
  const environment = {
    ...deploymentEnv(management, { identityMode: 'no-login', controlSealKey: SEAL_KEY }),
    ...realServerEnv('mcp', upstream, { mode: 'management' }),
  };
  const database = path.join(management, 'sessions.db');
  let server = null;

  try {
    // Q1: create the healthy authority only through the public API, then stop
    // before injecting one corrupt sibling into the disposable store.
    const installer = spawnServer('management', PORT, environment);
    server = installer.server;
    await waitForPort(PORT);
    const seeded = await client().beta.sessions.create({
      agent: 'coder',
      title: 'Canonical seed',
      metadata: { source: 'canonical' },
      environment_id: 'env_local',
      betas: BETAS,
    });
    await stopServer(server);
    server = null;

    const seedRows = sqliteJson(
      database,
      `SELECT scope_id, revision, aggregate_json
         FROM managed_session WHERE session_id = ${sqlQuote(seeded.id)}`,
    );
    assert.equal(seedRows.length, 1, 'Q1 healthy Session owns one durable row');
    assert.ok(seedRows[0].aggregate_json, 'Q1 healthy Session owns its canonical aggregate');
    insertMissingAggregateSession(database, seedRows[0].scope_id);

    // Q2: restart scans both rows. The corrupt one is isolated while the healthy
    // canonical aggregate remains readable and mutable through the official SDK.
    const reader = spawnServer('management', PORT, environment);
    server = reader.server;
    await waitForPort(PORT);
    let c = client();
    const firstQuarantine = await waitForValue(
      async () => sqliteJson(
        database,
        'SELECT session_id, reason FROM managed_session_quarantine ORDER BY session_id',
      ),
      (rows) => rows.length === 1,
      'the missing-aggregate Session to enter the exact durable quarantine',
      { timeoutMs: 10_000, pollMs: 20 },
    );
    const firstQuarantineEvidence = firstQuarantine.map(({ session_id, reason }) => [
      session_id,
      reason,
    ]);
    assert.deepEqual(firstQuarantineEvidence, [[
      CORRUPT_SESSION_ID,
      'managed Session aggregate is missing',
    ]]);

    const restored = await c.beta.sessions.retrieve(seeded.id, { betas: BETAS });
    assert.equal(restored.agent.id, 'coder');
    assert.equal(
      restored.agent.model.id,
      seeded.agent.model.id,
      'Q1 restart preserves the canonical model resolved at Session creation',
    );
    assert.equal(restored.environment_id, 'env_local');
    assert.equal(restored.title, 'Canonical seed');
    assert.equal(restored.metadata?.source, 'canonical');
    pass('Q1/Q2 canonical Session remains available beside one quarantined row');

    await assert.rejects(
      () => c.beta.sessions.retrieve(CORRUPT_SESSION_ID, { betas: BETAS }),
      (error) => {
        assert.equal(error?.status, 500);
        assert.equal(error?.error?.error?.type, 'api_error');
        assert.equal(
          error?.error?.error?.message,
          'Session repository contains corrupt durable state: managed Session aggregate is missing',
        );
        return true;
      },
    );
    const corruptRows = sqliteJson(
      database,
      `SELECT aggregate_json FROM managed_session WHERE session_id = ${sqlQuote(CORRUPT_SESSION_ID)}`,
    );
    assert.equal(corruptRows.length, 1, 'Q2 corrupt row remains present for exact diagnosis');
    assert.equal(
      corruptRows[0].aggregate_json,
      null,
      'Q2 corrupt projection columns never fabricate a Session aggregate',
    );
    pass('Q2 exact corrupt retrieve fails 500 and the missing aggregate stays missing');

    // Environment realization is an independent root mutation and may have
    // advanced the healthy revision. Measure immediately before this exact SDK
    // update so one title+metadata command still owns one revision transition.
    const beforeUpdateRows = sqliteJson(
      database,
      `SELECT revision FROM managed_session WHERE session_id = ${sqlQuote(seeded.id)}`,
    );
    assert.equal(beforeUpdateRows.length, 1);
    const revisionBeforeUpdate = beforeUpdateRows[0].revision;
    const updated = await c.beta.sessions.update(seeded.id, {
      title: 'Canonical title',
      metadata: { normalized: 'yes' },
      betas: BETAS,
    });
    assert.equal(updated.title, 'Canonical title');
    assert.equal(updated.metadata?.normalized, 'yes');
    await stopServer(server);
    server = null;

    const canonicalRows = sqliteJson(
      database,
      `SELECT revision, aggregate_json FROM managed_session WHERE session_id = ${sqlQuote(seeded.id)}`,
    );
    assert.equal(canonicalRows.length, 1);
    assert.equal(
      canonicalRows[0].revision,
      revisionBeforeUpdate + 1,
      'Q1 one canonical update advances exactly one root revision',
    );
    assert.ok(canonicalRows[0].aggregate_json, 'Q1 canonical update retains the sole aggregate');

    // Q4: poison only indexed/projection values. Graceful shutdown disposed the
    // local environment effect, so clear that orthogonal physical phase inside
    // the aggregate while preserving its authoritative public configuration.
    const canonicalAggregate = JSON.parse(canonicalRows[0].aggregate_json);
    canonicalAggregate.environment = { phase: 'unmaterialized' };
    canonicalAggregate.realization = null;
    sqlite(
      database,
      `UPDATE managed_session
         SET title = 'POISONED INDEX TITLE', model = 'poisoned-index-model',
             metadata_json = '{"source":"poisoned-index"}',
             aggregate_json = ${sqlQuote(JSON.stringify(canonicalAggregate))}
       WHERE session_id = ${sqlQuote(seeded.id)};`,
    );

    // Q3/Q4: a second cold process keeps one quarantine record, never fills the
    // corrupt aggregate, and reads the healthy Session only from aggregate_json.
    const canonical = spawnServer('management', PORT, environment);
    server = canonical.server;
    await waitForPort(PORT);
    c = client();
    const final = await c.beta.sessions.retrieve(seeded.id, { betas: BETAS });
    assert.equal(
      final.agent.model.id,
      seeded.agent.model.id,
      'Q4 poisoned model index cannot replace the canonical aggregate model',
    );
    assert.equal(final.title, 'Canonical title');
    assert.equal(final.metadata?.source, 'canonical');
    assert.equal(final.metadata?.normalized, 'yes');
    const secondQuarantineEvidence = sqliteJson(
      database,
      'SELECT session_id, reason FROM managed_session_quarantine ORDER BY session_id',
    ).map(({ session_id, reason }) => [session_id, reason]);
    assert.deepEqual(
      secondQuarantineEvidence,
      firstQuarantineEvidence,
      'Q3 restart preserves one exact quarantine record',
    );
    const restartedCorruptRows = sqliteJson(
      database,
      `SELECT aggregate_json FROM managed_session WHERE session_id = ${sqlQuote(CORRUPT_SESSION_ID)}`,
    );
    assert.equal(
      restartedCorruptRows.length,
      1,
      'Q3 restart preserves the exact corrupt row for diagnosis',
    );
    assert.equal(
      restartedCorruptRows[0].aggregate_json,
      null,
      'Q3 restart still does not fabricate the missing aggregate',
    );
    pass('Q3/Q4 quarantine is idempotent and canonical aggregate outranks poisoned indexes');

    console.log('E2E PASS: canonical Session aggregate quarantine and index-projection boundary.');
  } finally {
    if (server) await stopServer(server);
    upstream.close();
    fs.rmSync(management, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
