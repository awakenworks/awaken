// Durable Memory extraction recovery from every post-inference stage. The
// process is killed after a real terminal commit creates the original intent;
// persisted variants then prove Extracted/Stored resume and bounded failure.
//
// Test design. Causes: C1=terminal inference creates one extraction intent;
// C2=restart observes Prepared, Extracted, or Stored stage; C3=stored content is
// valid, conflicting, or permanently invalid; C4=a retry budget is exhausted.
// Effects: E1=each resumable stage advances once without repeating prior work;
// E2=the exact Memory commit is idempotent; E3=conflict/failure is classified and
// bounded without corrupting the source Run. Constraints/invariant: the durable
// intent stage/idempotency key and Memory repository are the only authorities.
// Decision rules: X1=C1+C2(valid)=>E1+E2; X2=X1+C3(conflict)=>E2;
// X3=C1+C2+C3(invalid)+C4=>E3.

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree,
  realServerEnv,
  scenarioMemoryStore,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';
import { nativeProviderCandidateFixture } from './fixtures/provider_candidate_fixture.mjs';
import { sqliteExec, sqliteRows } from './sqlite.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38244);
const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const STORE_DIR = `/tmp/awaken-memory-stage-recovery-${process.pid}`;

let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

function sha256(content) {
  return createHash('sha256').update(content).digest('hex');
}

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function sqlite(database, sql) {
  return sqliteExec(database, sql);
}

function extractionRows(database) {
  return sqliteRows(
    database,
    'SELECT intent_id, status, data FROM managed_memory_extraction ORDER BY intent_id',
  ).map((row) => ({ ...row, intent: JSON.parse(row.data) }));
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
  const rows = sqliteRows(
    database,
    `SELECT data FROM managed_memory_extraction WHERE intent_id=${sqlQuote(intentId)}`,
  );
  assert.equal(rows.length, 1, `missing extraction ${intentId}`);
  return rows[0].data;
}

function assistantText(events) {
  return events
    .filter((event) => event.type === 'agent.message')
    .map((event) => event.content.map((block) => block.text ?? '').join(''))
    .join('\n');
}

async function reply(sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return assistantText(events);
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
    headers: MEMORY_HEADERS,
  });
  return response;
}

async function main() {
  cleanupFixtureTree(STORE_DIR);
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const database = path.join(STORE_DIR, 'sessions.db');
  const servers = [];
  const upstream = await startUpstream('memory', { delayMs: 2_000 });
  try {
    const environment = {
      SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR,
      ...realServerEnv('memory', upstream, { mode: 'memory' }),
    };
    const first = spawnServer('memory', PORT, environment);
    servers.push(first.server);
    await waitForPort(PORT);

    const store = await scenarioMemoryStore(client, MEMORY_HEADERS);
    const oldUpdate = 'old update value';
    const already = 'already committed value';
    const updateHead = await createMemory(store.id, '/update.md', oldUpdate);
    await createMemory(store.id, '/already.md', already);
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
      resources: [{ type: 'memory_store', memory_store_id: store.id }],
    });
    // Initial Run cause/effect rules: I1 accepted User command => exact receipt
    // may be unprocessed while inference runs; I2 committed agent.message with
    // the recovery fact => terminal commit exists; I3 earlier-only history =>
    // keep reading; I4 the exact receipt is processed. Constraint K1: history
    // before that receipt cannot satisfy I2. Decision I1+I2+I4=>terminal fact;
    // only that rule may lead to the extraction-intent assertion.
    const initialSend = await client.beta.sessions.events.send(session.id, {
      betas: BETAS,
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'remember the staged recovery fact' }] }],
    });
    const initialReceipt = initialSend.data[0];
    assert.equal(initialReceipt.type, 'user.message');
    assert.equal(initialReceipt.processed_at, null);
    const initialObservation = await waitForSessionEventReceipt(
      client,
      session.id,
      initialReceipt.id,
      BETAS,
      ({ delta }) => /staged recovery fact/u.test(assistantText(delta)),
      'the initial Memory Run to commit its assistant fact',
    );
    const initialReply = assistantText(initialObservation.delta);
    assert.match(initialReply, /staged recovery fact/u);
    await waitForValue(
      () => fs.existsSync(database) ? extractionRows(database).length : 0,
      (count) => count === 1,
      'terminal commit to persist one extraction intent',
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

    // Extractor recovery decision table:
    // | durable stage | dependency condition | terminal effect              |
    // | Extracted     | heads unchanged      | store once, Completed         |
    // | Extracted     | head changed         | five attempts, TerminalFailed |
    // | Pending       | provider unreachable | five attempts, TerminalFailed |
    // | Stored        | receipt present      | finalize, Completed           |
    //
    // Mutate the authoritative current `extractor.agent` snapshot. Adding the
    // retained legacy `extractor.model` fields to a current snapshot is ignored
    // by its one-way decoder and would not exercise the intended failure.
    // Provider fixture rule X4: C5=Pending extraction carries complete explicit
    // but unreachable Provider coordinates with no credential -> E4=recovery
    // retries five times and records TerminalFailed without a provider effect.
    // Constraint/K: the shared helper supplies no defaults or validation;
    // ResolvedModelCandidate decoding is the sole structural-validity authority.
    const unavailableCandidate = nativeProviderCandidateFixture({
      binding: {
        ...source.extractor.agent.resolved_spec.model_binding,
        model_ref: 'unavailable-extractor-model',
      },
      providerRef: 'unavailable-provider',
      routeRef: 'unavailable-route',
      accessKind: 'direct',
      scopeId: source.workspace_id,
      credential: null,
      adapterKind: 'open_ai_chat',
      apiDialect: 'open_ai_chat',
      baseUrl: 'http://127.0.0.1:1/v1',
      upstreamModel: 'unavailable-extractor-model',
    });
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
        agent: {
          ...source.extractor.agent,
          resolved_spec: {
            ...source.extractor.agent.resolved_spec,
            model_binding: unavailableCandidate,
          },
        },
      },
    };
    persistIntent(database, unavailableExtractor, true);

    const second = spawnServer('memory', PORT, environment);
    servers.push(second.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
    assert.match(await reply(session.id), /staged recovery fact/u);

    await waitForValue(
      () => extractionRows(database),
      (rows) => rows.length === 4 && rows.every(({ status }) =>
        status === 'completed' || status === 'terminal_failed'),
      'staged extraction intents to reach terminal states',
    );
    const rows = new Map(extractionRows(database).map((row) => [row.intent.intent_id, row.intent]));
    assert.equal(rows.get(extracted.intent_id).status, 'completed');
    assert.equal(rows.get(stored.intent_id).status, 'completed');
    assert.equal(rows.get(conflict.intent_id).status, 'terminal_failed');
    assert.equal(rows.get(conflict.intent_id).attempts, 5);
    assert.match(rows.get(conflict.intent_id).last_error, /changed after extraction planning/u);
    assert.equal(rows.get(unavailableExtractor.intent_id).status, 'terminal_failed');
    assert.equal(rows.get(unavailableExtractor.intent_id).attempts, 5);
    assert.match(
      rows.get(unavailableExtractor.intent_id).last_error,
      /requires an installed credential materializer/u,
    );

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
      {
        ...cleanPending,
        extractor: {
          ...cleanPending.extractor,
          agent: { ...cleanPending.extractor.agent, root_agent_id: ' ' },
        },
      },
      {
        ...cleanPending,
        extractor: {
          ...cleanPending.extractor,
          agent: {
            ...cleanPending.extractor.agent,
            resolved_spec: {
              ...cleanPending.extractor.agent.resolved_spec,
              model_binding: {
                ...cleanPending.extractor.agent.resolved_spec.model_binding,
                model_ref: ' ',
              },
            },
          },
        },
      },
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
      await hardKill(servers.pop());
      const probe = spawnServer('memory', PORT, environment);
      servers.push(probe.server);
      await waitForPort(PORT);
      client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
      assert.match(
        await reply(session.id),
        /staged recovery fact/u,
        'a corrupt extraction aggregate leaves the committed Session readable',
      );
      assert.equal(
        rawIntent(database, validCompleted.intent_id),
        data,
        'a corrupt extraction aggregate must not be claimed or rewritten',
      );
      assert.equal(probe.server.exitCode, null, 'corrupt extraction crashed the process');
    }
    persistIntent(database, validCompleted, false);

    const memories = await client.get(`/v1/memory_stores/${store.id}/memories?view=full`, {
      headers: MEMORY_HEADERS,
    });
    const byPath = new Map(memories.data.map((memory) => [memory.path, memory]));
    assert.equal(byPath.get('/created.md').content, createContent);
    assert.equal(byPath.get('/update.md').content, updateContent);
    assert.equal(byPath.get('/already.md').content, already);
    assert.equal(byPath.size, 3);

    console.log('E2E PASS: Extracted/Stored resume and bounded extraction failures converge durably.');
  } finally {
    for (const server of servers) await stopServer(server);
    upstream.close();
    cleanupFixtureTree(STORE_DIR);
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
