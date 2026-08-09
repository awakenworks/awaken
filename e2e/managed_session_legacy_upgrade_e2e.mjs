// Retained Session-row upgrade through real process boundaries.
//
// Cause graph:
//   aggregate_json present ───────────────► decode canonical aggregate
//   aggregate_json absent
//     ├─ valid legacy columns ─────────────► compile one frozen baseline
//     │                                     └─ next root mutation writes aggregate_json
//     ├─ valid terminal legacy row ─────────► decode, but never resurrect
//     └─ later legacy-column drift ─────────► ignored once aggregate_json exists
//
// Decision table:
// | Rule | aggregate | legacy valid | root mutation | later legacy drift | authority/result |
// | L1   | absent    | yes          | no            | -                  | legacy is decoded |
// | L2   | absent    | yes          | yes           | -                  | aggregate is written once |
// | L3   | present   | irrelevant   | -             | yes                | aggregate wins |
// | L4   | absent    | terminal     | no            | -                  | not found/no effect |
//
// Constraint: every row rule runs with explicit `identity_mode=no-login`; IAM is
// orthogonal to retained-row decoding and therefore must not mask a codec result.
//
// This is deliberately a process/API test rather than a direct row-codec test:
// SQLite is only the retained compatibility input; retrieval and mutation use
// the official Managed Agents TypeScript SDK surface.

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
} from './harness.mjs';
import { sqliteExec, sqliteRows } from './sqlite.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38671);
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
let sessionId = '';

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function sqlite(database, sql) {
  return sqliteExec(database, sql);
}

function sqliteJson(database, sql) {
  return sqliteRows(database, sql);
}

function rewriteAsLegacySession(database) {
  const runtime = {
    mcp_servers: [],
    delegate_ids: ['legacy-delegate'],
    runtime: null,
    deny_egress: false,
    // Environment-binding migration has its own continuity E2E. Keeping this
    // row unmaterialized isolates the row-codec decision table from an
    // unrelated local sandbox that a graceful process stop must dispose.
    sandbox: null,
  };
  sqlite(database, `UPDATE managed_session SET
      agent_id = 'coder', model = 'management', title = 'Legacy title',
      metadata_json = '{"source":"legacy"}', environment_id = 'env_local',
      mcp_json = '[]', status = 'idle', archived_at = NULL,
      effective_inputs_json = '{"inputs":[]}', environment_binding = NULL,
      runtime_json = ${sqlQuote(JSON.stringify(runtime))}, revision = 7,
      aggregate_json = NULL
    WHERE session_id = ${sqlQuote(sessionId)};`);
}

function insertTerminalLegacySession(database, scopeId) {
  const runtime = {
    mcp_servers: [
      {
        name: 'legacy-public-client',
        url: 'https://public.example.test/mcp',
        credential_source_id: 'cred-public',
        credential_revision: 11,
        refresh: {
          token_endpoint: 'https://auth.example.test/public/token',
          client_id: 'public-client',
          refresh_token_ref: 'secret:refresh:public',
          token_endpoint_auth: 'None',
          scope: 'read',
          resource: null,
        },
      },
      {
        name: 'legacy-basic-client',
        url: 'https://basic.example.test/mcp',
        credential_source_id: 'cred-basic',
        credential_revision: 12,
        refresh: {
          token_endpoint: 'https://auth.example.test/basic/token',
          client_id: 'basic-client',
          refresh_token_ref: 'secret:refresh:basic',
          token_endpoint_auth: {
            ClientSecretBasic: { secret_ref: 'secret:client:basic' },
          },
          scope: null,
          resource: 'https://basic.example.test/',
        },
      },
      {
        name: 'legacy-post-client',
        url: 'https://post.example.test/mcp',
        credential_source_id: 'cred-post',
        credential_revision: 13,
        refresh: {
          token_endpoint: 'https://auth.example.test/post/token',
          client_id: 'post-client',
          refresh_token_ref: 'secret:refresh:post',
          token_endpoint_auth: {
            ClientSecretPost: { secret_ref: 'secret:client:post' },
          },
          scope: 'write',
          resource: 'https://post.example.test/',
        },
      },
    ],
    delegate_ids: ['legacy-acp-delegate'],
    runtime: 'acp:claude',
    deny_egress: true,
    sandbox: null,
  };
  const resourceState = {
    revision: 0,
    active: { inputs: [] },
    pending: null,
    activations: [],
  };
  sqlite(database, `INSERT INTO managed_session (
      session_id, agent_id, model, title, metadata_json, environment_id,
      mcp_json, scope_id, status, archived_at, effective_inputs_json,
      environment_binding, runtime_json, revision, aggregate_json
    ) VALUES (
      'sesn_legacy_terminal', 'coder', 'management', 'Terminal legacy', '{}',
      'env_local', '[]', ${sqlQuote(scopeId)}, 'activation_failed', NULL,
      ${sqlQuote(JSON.stringify(resourceState))}, NULL,
      ${sqlQuote(JSON.stringify(runtime))}, 3, NULL
    );`);
}

function client() {
  return new Anthropic({
    apiKey: 'e2e-dummy',
    baseURL: `http://127.0.0.1:${PORT}`,
  });
}

async function main() {
  const management = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-session-upgrade-mgmt-'));
  const upstream = await startUpstream('mcp');
  const environment = {
    ...deploymentEnv(management, { identityMode: 'no-login', controlSealKey: SEAL_KEY }),
    ...realServerEnv('mcp', upstream, { mode: 'management' }),
  };
  // One typed data root owns both the Session aggregate and Runtime committed
  // truth; the retained upgrade never relies on a second management directory.
  const database = path.join(management, 'sessions.db');
  let server = null;

  try {
    // Lifetime A creates reachable historical truth through the public API. We
    // later rewrite only its retained config row; the Runtime committed facts
    // remain exactly what a pre-ADR-66 process would have produced.
    const installer = spawnServer('management', PORT, environment);
    server = installer.server;
    await waitForPort(PORT);
    const seeded = await client().beta.sessions.create({
      agent: 'coder',
      title: 'Seed title',
      environment_id: 'env_local',
      betas: BETAS,
    });
    sessionId = seeded.id;
    await stopServer(server);
    server = null;
    rewriteAsLegacySession(database);
    const ownerRows = sqliteJson(
      database,
      `SELECT scope_id FROM managed_session WHERE session_id = ${sqlQuote(sessionId)}`,
    );
    insertTerminalLegacySession(database, ownerRows[0].scope_id);

    // L1: a replacement process reads the retained row through the production
    // repository and exposes the normalized frozen Session through the SDK.
    const reader = spawnServer('management', PORT, environment);
    server = reader.server;
    await waitForPort(PORT);
    let c = client();
    const restored = await c.beta.sessions.retrieve(sessionId, { betas: BETAS });
    assert.equal(restored.agent.id, 'coder');
    assert.equal(restored.agent.model.id, 'management');
    assert.equal(restored.environment_id, 'env_local');
    assert.equal(restored.title, 'Legacy title');
    assert.equal(restored.metadata?.source, 'legacy');
    pass('L1 retained Session columns compile into one frozen aggregate view');

    // Environment realization is an independent canonical root mutation and
    // may race this compatibility read. The update contract is therefore
    // measured from the latest durable revision, never from the synthetic
    // legacy fixture's revision 7.
    const beforeUpdateRows = sqliteJson(
      database,
      `SELECT revision FROM managed_session WHERE session_id = ${sqlQuote(sessionId)}`,
    );
    assert.equal(beforeUpdateRows.length, 1);
    const revisionBeforeUpdate = beforeUpdateRows[0].revision;

    await assert.rejects(
      () => c.beta.sessions.retrieve('sesn_legacy_terminal', { betas: BETAS }),
      (error) => error?.status === 404,
    );
    pass('L4 terminal ACP/OAuth legacy bindings decode but cannot resurrect');

    // L2: one ordinary root command atomically applies title + metadata across
    // the one-way write boundary. It advances one canonical revision and must
    // not maintain a synchronized second copy in the legacy columns.
    const updated = await c.beta.sessions.update(sessionId, {
      title: 'Canonical title',
      metadata: { normalized: 'yes' },
      betas: BETAS,
    });
    assert.equal(updated.title, 'Canonical title');
    assert.equal(updated.metadata?.normalized, 'yes');
    await stopServer(server);
    server = null;

    const rows = sqliteJson(
      database,
      `SELECT revision, aggregate_json, title, model FROM managed_session WHERE session_id = ${sqlQuote(sessionId)}`,
    );
    assert.equal(rows.length, 1);
    assert.equal(
      rows[0].revision,
      revisionBeforeUpdate + 1,
      'one title + metadata command advances exactly one canonical revision',
    );
    assert.ok(rows[0].aggregate_json, 'root mutation persisted the canonical aggregate');
    assert.equal(rows[0].title, 'Legacy title', 'legacy title column is no longer synchronized');
    pass('L2 root mutations write aggregate_json without a parallel legacy write');

    // Simulate stale legacy storage after the migration. If a dual-read path
    // survives, the next process would expose these poisoned values. The reader
    // process legitimately realized its local environment; clear that orthogonal
    // ephemeral binding while offline so L3 tests only aggregate-vs-column
    // authority and does not ask strict Managed restoration to recreate a
    // sandbox that graceful shutdown just disposed. The realization lease fences
    // that same physical effect, so the offline fixture must clear both values;
    // clearing only the binding creates an impossible half-state (new effect
    // requested while the old owner is still asserted current).
    const canonicalAggregate = JSON.parse(rows[0].aggregate_json);
    canonicalAggregate.environment = { phase: 'unmaterialized' };
    canonicalAggregate.realization = null;
    sqlite(
      database,
      `UPDATE managed_session
         SET title = 'POISONED LEGACY TITLE', model = 'poisoned-model',
             metadata_json = '{"source":"poisoned"}',
             aggregate_json = ${sqlQuote(JSON.stringify(canonicalAggregate))}
       WHERE session_id = ${sqlQuote(sessionId)};`,
    );

    // L3: aggregate_json is now the sole authority across another process.
    const canonical = spawnServer('management', PORT, environment);
    server = canonical.server;
    await waitForPort(PORT);
    c = client();
    const final = await c.beta.sessions.retrieve(sessionId, { betas: BETAS });
    assert.equal(final.agent.model.id, 'management');
    assert.equal(final.title, 'Canonical title');
    assert.equal(final.metadata?.source, 'legacy');
    assert.equal(final.metadata?.normalized, 'yes');
    pass('L3 canonical aggregate ignores stale retained columns after restart');

    console.log('E2E PASS: retained Session rows upgrade once to the canonical aggregate.');
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
