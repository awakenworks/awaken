// Crash/restart coverage for malformed durable ACP permission tickets. Damage
// is injected only into the throwaway SQLite authority while the process is
// stopped; recovery is driven through the normal Managed API.

import assert from 'node:assert/strict';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import type {
  BetaManagedAgentsSessionEvent,
  BetaManagedAgentsUserToolConfirmationEventParams,
} from '@anthropic-ai/sdk/resources/beta/sessions/events';
// @ts-ignore -- shared JavaScript ACP fixture intentionally serves TS scenarios.
import { startAcpPermissionAwait } from './fixtures/acp_permission_await.mjs';
// @ts-ignore -- shared JavaScript harness intentionally serves TS scenarios.
import { pass, spawnServer, stopServer, waitForPort } from './harness.mjs';
// @ts-ignore -- shared JavaScript SQLite fixture intentionally serves TS scenarios.
import { sqliteDatabaseForThread, sqliteRows, sqliteRun } from './sqlite.mjs';

const PORT = Number(process.env.E2E_PORT ?? 39773);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];

async function events(
  client: Anthropic,
  sessionId: string,
): Promise<BetaManagedAgentsSessionEvent[]> {
  const observed: BetaManagedAgentsSessionEvent[] = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    observed.push(event);
  }
  return observed;
}

function rewriteTicket(
  storage: string,
  sessionId: string,
  rewrite: (ticket: any) => void,
): { database: string; runId: string } {
  const database = sqliteDatabaseForThread(storage, sessionId, 'runtime_waiting');
  const row = sqliteRows(
    database,
    'SELECT run_id || char(9) || ticket FROM runtime_waiting LIMIT 1',
  )[0] as Record<string, unknown> | undefined;
  const encoded = row ? String(Object.values(row)[0]) : '';
  const separator = encoded.indexOf('\t');
  assert.ok(separator > 0, `ACP permission ticket exists for ${sessionId}: ${encoded}`);
  const runId = encoded.slice(0, separator);
  const ticket = JSON.parse(encoded.slice(separator + 1));
  const before = JSON.stringify(ticket);
  rewrite(ticket);
  const after = JSON.stringify(ticket);
  assert.notEqual(after, before, 'fault injection must change the durable ticket bytes');
  const serialized = after.replaceAll("'", "''");
  const changed = sqliteRun(
    database,
    `UPDATE runtime_waiting SET ticket = '${serialized}' WHERE run_id = '${runId.replaceAll("'", "''")}'`,
  );
  assert.equal(Number(changed.changes), 1);
  return { database, runId };
}

async function expectCorruptTicketFailure(
  client: Anthropic,
  sessionId: string,
  toolId: string,
  database: string,
  runId: string,
): Promise<void> {
  const confirmation: BetaManagedAgentsUserToolConfirmationEventParams = {
    type: 'user.tool_confirmation',
    tool_use_id: toolId,
    result: 'allow',
  };
  await assert.rejects(
    client.beta.sessions.events.send(sessionId, {
      events: [confirmation],
      betas: BETAS,
    }),
    (error: any) => error?.status === 500,
  );
  await assert.rejects(events(client, sessionId), (error: any) => error?.status === 500);
  assert.equal(
    Number(
      sqliteRows(
        database,
        'SELECT COUNT(*) AS count FROM runtime_waiting WHERE run_id = ?',
        runId,
      )[0]?.count,
    ),
    1,
    'the rejected recovery leaves the sole corrupt waiting ticket unconsumed',
  );
  const messages = sqliteRows(
    database,
    'SELECT data FROM runtime_message WHERE thread_id = ? ORDER BY id',
    sessionId,
  );
  assert.ok(
    !JSON.stringify(messages).includes('ACP-PERMISSION-ALLOWED'),
    'the rejected recovery never executes the pending permission tool',
  );
}

async function main(): Promise<void> {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-acp-ticket-'));
  const environment = { SESSION_DEPLOYMENT_STORAGE_DIR: storage, SESSION_DEPLOYMENT_INGRESS: 'durable' };
  let server = spawnServer('acp-permission', PORT, environment).server;
  try {
    await waitForPort(PORT, 180_000, server);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });
    const malformed = await startAcpPermissionAwait(client, BETAS);

    // Cause/effect graph: C1 a closed ToolCall permission target is committed;
    // C2 its required nested tool is removed while the process is stopped; C3
    // the exact qualified public tool id is confirmed after restart. Effects:
    // E1 the mutation changes retained bytes; E2 send/list both fail 500 before
    // a confirmation receipt can commit; E3 the waiting row remains and the
    // pending command never executes. Decision rule T1=C1+C2+C3=>E1+E2+E3.
    // Constraints/invariants: AwaitTarget serde/recovery is the only structural
    // authority; other missing nested fields belong to the same serde failure
    // class, so this system boundary keeps one representative instead of a
    // duplicate corruption path.
    await stopServer(server);
    const corruption = rewriteTicket(storage, malformed.session.id, (ticket) => {
      const target = ticket?.target?.ToolCall;
      assert.equal(target?.reason, 'Permission', 'fixture mutates the closed permission target');
      assert.ok(target?.call_id, 'closed permission target carries its call identity');
      assert.ok(target?.tool, 'closed permission target carries its pending tool');
      delete target.tool;
    });

    server = spawnServer('acp-permission', PORT, environment).server;
    await waitForPort(PORT, 180_000, server);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });
    await expectCorruptTicketFailure(
      client,
      malformed.session.id,
      malformed.tool.id,
      corruption.database,
      corruption.runId,
    );
    pass('restart rejects a malformed closed permission target without consuming or executing it');

    console.log('ACP PERMISSION CORRUPTION TS API E2E PASS.');
  } finally {
    await stopServer(server).catch(() => {});
    fs.rmSync(storage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('ACP PERMISSION CORRUPTION TS API E2E FAIL:', error);
  process.exitCode = 1;
});
