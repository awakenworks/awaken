// Durable Memory extraction recovery from every post-inference stage. The
// process is killed after a real terminal commit creates the original intent;
// persisted variants then prove Extracted/Stored resume and bounded failure.

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import Anthropic from '@anthropic-ai/sdk';
import {
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38244);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-memory-stage-recovery-${process.pid}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

function sha256(content) {
  return createHash('sha256').update(content).digest('hex');
}

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function sqlite(database, sql) {
  return execFileSync('sqlite3', [database], { input: sql, encoding: 'utf8' });
}

function extractionRows(database) {
  const output = execFileSync('sqlite3', [
    '-cmd',
    '.timeout 5000',
    '-json',
    database,
    'SELECT intent_id, status, data FROM managed_memory_extraction ORDER BY intent_id',
  ]).toString().trim();
  return output ? JSON.parse(output).map((row) => ({ ...row, intent: JSON.parse(row.data) })) : [];
}

function persistIntent(database, intent, insert) {
  const data = JSON.stringify(intent);
  if (insert) {
    sqlite(
      database,
      `INSERT INTO managed_memory_extraction(
        intent_id, idempotency_key, status, revision, lease_expires_at_unix_ms, data
      ) VALUES (
        ${sqlQuote(intent.intent_id)}, ${sqlQuote(intent.idempotency_key)},
        ${sqlQuote(intent.status)}, ${intent.revision}, NULL, ${sqlQuote(data)}
      );`,
    );
    return;
  }
  sqlite(
    database,
    `UPDATE managed_memory_extraction SET
      status=${sqlQuote(intent.status)}, revision=${intent.revision},
      lease_expires_at_unix_ms=NULL, data=${sqlQuote(data)}
     WHERE intent_id=${sqlQuote(intent.intent_id)};`,
  );
}

function persistRecoverableRaw(database, intentId, data) {
  sqlite(
    database,
    `UPDATE managed_memory_extraction SET
      status='pending', lease_expires_at_unix_ms=NULL, data=${sqlQuote(data)}
     WHERE intent_id=${sqlQuote(intentId)};`,
  );
}

function rawIntent(database, intentId) {
  const output = execFileSync('sqlite3', [
    '-json',
    database,
    `SELECT data FROM managed_memory_extraction WHERE intent_id=${sqlQuote(intentId)}`,
  ]).toString().trim();
  const rows = output ? JSON.parse(output) : [];
  assert.equal(rows.length, 1, `missing extraction ${intentId}`);
  return rows[0].data;
}

async function reply(sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return events
    .filter((event) => event.type === 'agent.message')
    .map((event) => event.content.map((block) => block.text ?? '').join(''))
    .join('\n');
}

async function turn(sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  return reply(sessionId);
}

async function waitUntil(predicate, message, tries = 160) {
  for (let attempt = 0; attempt < tries; attempt += 1) {
    if (await predicate()) return;
    await sleep(100);
  }
  assert.fail(message);
}

async function hardKill(server) {
  if (server.exitCode !== null || server.signalCode !== null) return;
  const exited = new Promise((resolve) => server.once('exit', resolve));
  server.kill('SIGKILL');
  await exited;
}

async function createMemory(storeId, pathName, content) {
  const response = await client.post(`/v1/memory_stores/${storeId}/memories`, {
    body: { path: pathName, content },
  });
  return response;
}

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const database = path.join(STORE_DIR, 'sessions.db');
  const servers = [];
  const upstream = await startUpstream('memory', { delayMs: 2_000 });
  try {
    const environment = {
      AWAKEN_STORAGE_DIR: STORE_DIR,
      ...realServerEnv('memory', upstream, { mode: 'memory' }),
    };
    const first = spawnServer('memory', PORT, environment);
    servers.push(first.server);
    await waitForPort(PORT);

    const store = await client.post('/v1/memory_stores', { body: { name: 'stage-recovery' } });
    const oldUpdate = 'old update value';
    const already = 'already committed value';
    const updateHead = await createMemory(store.id, '/update.md', oldUpdate);
    await createMemory(store.id, '/already.md', already);
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      betas: BETAS,
      resources: [{ type: 'memory_store', memory_store_id: store.id, mount_path: '/memory' }],
    });
    assert.match(await turn(session.id, 'remember the staged recovery fact'), /staged recovery fact/u);
    await waitUntil(
      () => fs.existsSync(database) && extractionRows(database).length === 1,
      'terminal commit did not persist an extraction intent',
    );
    await hardKill(first.server);
    servers.pop();

    const [sourceRow] = extractionRows(database);
    const source = sourceRow.intent;
    const createContent = 'created from durable Extracted state';
    const updateContent = 'updated from durable Extracted state';
    const extractedMutations = [
      {
        path: '/created.md',
        content: createContent,
        observed_sha256: null,
        target_sha256: sha256(createContent),
      },
      {
        path: '/update.md',
        content: updateContent,
        observed_sha256: updateHead.content_sha256,
        target_sha256: sha256(updateContent),
      },
      {
        path: '/already.md',
        content: already,
        observed_sha256: 'unused-after-idempotency-match',
        target_sha256: sha256(already),
      },
    ];
    const unclaimed = {
      ...source,
      claim_owner: null,
      lease_expires_at_unix_ms: null,
      last_error: null,
    };
    const extracted = {
      ...unclaimed,
      status: 'extracted',
      mutations: extractedMutations,
      receipt: null,
    };
    persistIntent(database, extracted, false);

    const stored = {
      ...unclaimed,
      intent_id: `${source.intent_id}:stored-resume`,
      idempotency_key: `${source.idempotency_key}:stored-resume`,
      terminal_commit_id: `${source.terminal_commit_id}:stored-resume`,
      status: 'stored',
      mutations: extractedMutations,
      receipt: {
        stored_at_unix_ms: 1,
        mutations: extractedMutations.map((mutation) => ({
          path: mutation.path,
          target_sha256: mutation.target_sha256,
          already_applied: true,
        })),
      },
    };
    persistIntent(database, stored, true);

    const conflictContent = 'must never overwrite concurrent content';
    const conflict = {
      ...unclaimed,
      intent_id: `${source.intent_id}:cas-conflict`,
      idempotency_key: `${source.idempotency_key}:cas-conflict`,
      terminal_commit_id: `${source.terminal_commit_id}:cas-conflict`,
      status: 'extracted',
      attempts: 0,
      mutations: [{
        path: '/update.md',
        content: conflictContent,
        observed_sha256: 'stale-observed-sha256',
        target_sha256: sha256(conflictContent),
      }],
      receipt: null,
    };
    persistIntent(database, conflict, true);

    const unavailableExtractor = {
      ...unclaimed,
      intent_id: `${source.intent_id}:unavailable-extractor`,
      idempotency_key: `${source.idempotency_key}:unavailable-extractor`,
      terminal_commit_id: `${source.terminal_commit_id}:unavailable-extractor`,
      status: 'pending',
      attempts: 0,
      mutations: [],
      receipt: null,
      extractor: {
        ...source.extractor,
        model_ref: 'unavailable-extractor-model',
      },
    };
    persistIntent(database, unavailableExtractor, true);

    const second = spawnServer('memory', PORT, environment);
    servers.push(second.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
    await client.beta.sessions.events.send(session.id, { betas: BETAS, events: [] });
    assert.match(await reply(session.id), /staged recovery fact/u);

    await waitUntil(() => {
      const rows = extractionRows(database);
      return rows.length === 4 && rows.every(({ status }) =>
        status === 'completed' || status === 'terminal_failed');
    }, 'staged extraction intents did not reach terminal states');
    const rows = new Map(extractionRows(database).map((row) => [row.intent.intent_id, row.intent]));
    assert.equal(rows.get(extracted.intent_id).status, 'completed');
    assert.equal(rows.get(stored.intent_id).status, 'completed');
    assert.equal(rows.get(conflict.intent_id).status, 'terminal_failed');
    assert.equal(rows.get(conflict.intent_id).attempts, 5);
    assert.match(rows.get(conflict.intent_id).last_error, /changed after extraction planning/u);
    assert.equal(rows.get(unavailableExtractor.intent_id).status, 'terminal_failed');
    assert.equal(rows.get(unavailableExtractor.intent_id).attempts, 5);
    assert.match(rows.get(unavailableExtractor.intent_id).last_error, /pinned inference access/u);

    // The durable repository validates the whole staged aggregate on every
    // recoverable read. Inject malformed lifecycle combinations through SQLite
    // while the real reconciler is running: each row must remain untouched and
    // the process must keep serving, rather than resume from invented defaults.
    const validCompleted = rows.get(stored.intent_id);
    const validReceipt = validCompleted.receipt;
    const validMutation = validCompleted.mutations[0];
    const cleanPending = {
      ...validCompleted,
      status: 'pending',
      mutations: [],
      receipt: null,
      claim_owner: null,
      lease_expires_at_unix_ms: null,
    };
    const corruptions = [
      { ...cleanPending, intent_id: ' ' },
      { ...cleanPending, idempotency_key: ' ' },
      { ...cleanPending, workspace_id: ' ' },
      { ...cleanPending, session_id: ' ' },
      { ...cleanPending, terminal_commit_id: ' ' },
      { ...cleanPending, memory_store_id: ' ' },
      { ...cleanPending, extractor: { ...cleanPending.extractor, agent_id: ' ' } },
      { ...cleanPending, extractor: { ...cleanPending.extractor, model_ref: ' ' } },
      { ...cleanPending, memory_config_version: 0 },
      { ...cleanPending, mutations: [validMutation] },
      { ...cleanPending, status: 'extracted', mutations: [validMutation], receipt: validReceipt },
      { ...cleanPending, status: 'stored', mutations: [validMutation], receipt: null },
      {
        ...cleanPending,
        status: 'stored',
        mutations: [validMutation],
        receipt: {
          ...validReceipt,
          mutations: [{ ...validReceipt.mutations[0], target_sha256: 'forged-target' }],
        },
      },
      {
        ...cleanPending,
        status: 'terminal_failed',
        claim_owner: 'stale-worker',
        lease_expires_at_unix_ms: Number.MAX_SAFE_INTEGER,
      },
      {
        ...cleanPending,
        status: 'extracted',
        mutations: [{ ...validMutation, path: ' ', target_sha256: ' ' }],
      },
    ];
    for (const corrupt of corruptions) {
      const data = JSON.stringify(corrupt);
      persistRecoverableRaw(database, validCompleted.intent_id, data);
      await sleep(800);
      assert.equal(
        rawIntent(database, validCompleted.intent_id),
        data,
        'a corrupt extraction aggregate must not be claimed or rewritten',
      );
      assert.equal(second.server.exitCode, null, 'corrupt extraction crashed the process');
    }
    persistIntent(database, validCompleted, false);

    const memories = await client.get(`/v1/memory_stores/${store.id}/memories`);
    const byPath = new Map(memories.data.map((memory) => [memory.path, memory]));
    assert.equal(byPath.get('/created.md').content, createContent);
    assert.equal(byPath.get('/update.md').content, updateContent);
    assert.equal(byPath.get('/already.md').content, already);
    assert.equal(byPath.size, 3);

    console.log('E2E PASS: Extracted/Stored resume and bounded extraction failures converge durably.');
  } finally {
    for (const server of servers) await stopServer(server);
    upstream.close();
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
