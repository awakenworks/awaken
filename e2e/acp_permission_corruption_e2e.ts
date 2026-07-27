// Crash/restart coverage for malformed durable ACP permission tickets. Damage
// is injected only into the throwaway SQLite authority while the process is
// stopped; recovery is driven through the normal Managed API.

import assert from 'node:assert/strict';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
// @ts-ignore -- shared JavaScript harness intentionally serves TS scenarios.
import { pass, spawnServer, stopServer, waitForPort } from './harness.mjs';
// @ts-ignore -- shared JavaScript SQLite fixture intentionally serves TS scenarios.
import { sqliteRows, sqliteRun } from './sqlite.mjs';

const PORT = Number(process.env.E2E_PORT ?? 39773);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];

async function events(client: Anthropic, sessionId: string): Promise<any[]> {
  const observed: any[] = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    observed.push(event);
  }
  return observed;
}

async function startAwaiting(client: Anthropic): Promise<string> {
  const session = await client.beta.sessions.create({
    agent: 'acp-agent',
    environment_id: 'env_local',
    betas: BETAS,
  });
  await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'request permission' }] }],
    betas: BETAS,
  });
  assert.ok((await events(client, session.id)).some((event) => event.id === 'permission-call'));
  return session.id;
}

function rewriteTicket(storage: string, sessionId: string, rewrite: (ticket: any) => void): void {
  const database = path.join(storage, `${sessionId}.db`);
  const row = sqliteRows(
    database,
    'SELECT run_id || char(9) || ticket FROM runtime_waiting LIMIT 1',
  )[0] as Record<string, unknown> | undefined;
  const encoded = row ? String(Object.values(row)[0]) : '';
  const separator = encoded.indexOf('\t');
  assert.ok(separator > 0, `ACP permission ticket exists for ${sessionId}: ${encoded}`);
  const runId = encoded.slice(0, separator);
  const ticket = JSON.parse(encoded.slice(separator + 1));
  rewrite(ticket);
  const serialized = JSON.stringify(ticket).replaceAll("'", "''");
  const changed = sqliteRun(
    database,
    `UPDATE runtime_waiting SET ticket = '${serialized}' WHERE run_id = '${runId.replaceAll("'", "''")}'`,
  );
  assert.equal(Number(changed.changes), 1);
}

async function expectDecisionFailure(client: Anthropic, sessionId: string, status: number): Promise<void> {
  await assert.rejects(
    client.beta.sessions.events.send(sessionId, {
      events: [
        {
          type: 'user.tool_confirmation',
          tool_use_id: 'permission-call',
          result: 'allow',
        },
      ],
      betas: BETAS,
    }),
    (error: any) => error?.status === status,
  );
  assert.ok(!JSON.stringify(await events(client, sessionId)).includes('ACP-PERMISSION-ALLOWED'));
}

async function main(): Promise<void> {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-acp-ticket-'));
  const environment = { SESSION_DEPLOYMENT_STORAGE_DIR: storage, SESSION_DEPLOYMENT_INGRESS: 'durable' };
  let server = spawnServer('acp-permission', PORT, environment).server;
  try {
    await waitForPort(PORT, 180_000, server);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });
    const missingCall = await startAwaiting(client);
    const missingTool = await startAwaiting(client);

    await stopServer(server);
    rewriteTicket(storage, missingCall, (ticket) => delete ticket.call_id);
    rewriteTicket(storage, missingTool, (ticket) => delete ticket.pending_tool);

    server = spawnServer('acp-permission', PORT, environment).server;
    await waitForPort(PORT, 180_000, server);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });
    await expectDecisionFailure(client, missingCall, 400);
    await expectDecisionFailure(client, missingTool, 500);
    pass('restart rejects permission tickets missing call identity or pending tool');

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
