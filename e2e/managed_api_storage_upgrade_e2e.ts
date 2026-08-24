// Managed API compatibility across a real process restart and a frozen
// pre-typed-storage boundary.
//
// Cause/effect decision table:
// | Rule | durable input | process | observable effect |
// | U1 | dotted legacy Deployment event | restarted | SDK reads official dotted event |
// | U2 | pre-event-batch Session aggregate | restarted | SDK reads Session, no quarantine |
// | U3 | U1/U2 then public updates | stopped | current versioned/internal storage encoding |
// | U4 | current rewritten rows | restarted again | both SDK resources remain readable |
// | U5 | internal snake_case API request | live | 400 before persistence |

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
// @ts-ignore shared E2E harness is intentionally JavaScript.
import { cleanupFixtureTree, deploymentEnv, pass, realServerEnv, spawnServer, startUpstream, stopServer, waitForPort } from './harness.mjs';
// @ts-ignore Node SQLite helper is intentionally JavaScript.
import { sqliteRows, sqliteRun } from './sqlite.mjs';

const BETAS = ['managed-agents-2026-04-01'] as const;
const PORT = Number(process.env.E2E_PORT ?? 38_675);
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';

const sdk = () => new Anthropic({
  apiKey: 'e2e-dummy',
  baseURL: `http://127.0.0.1:${PORT}`,
  maxRetries: 0,
});

async function main() {
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-managed-upgrade-'));
  const sandboxDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-managed-upgrade-runs-'));
  const upstream = await startUpstream('mcp');
  const environment = {
    ...deploymentEnv(dataDir, { identityMode: 'no-login', controlSealKey: SEAL_KEY }),
    ...realServerEnv('mcp', upstream, { mode: 'management' }),
    SESSION_DEPLOYMENT_STORAGE_DIR: sandboxDir,
  };
  const database = path.join(dataDir, 'sessions.db');
  let server: ReturnType<typeof spawnServer>['server'] | null = null;

  try {
    const writer = spawnServer('management', PORT, environment);
    server = writer.server;
    await waitForPort(PORT, 900_000, server);
    let client = sdk();
    const agent = await client.beta.agents.create({
      name: 'upgrade-agent',
      model: 'claude-opus-4-8',
      betas: [...BETAS],
    });
    const executionEnvironment = await client.beta.environments.create({
      name: 'upgrade-environment',
      config: { type: 'cloud' },
      betas: [...BETAS],
    });
    const session = await client.beta.sessions.create({
      agent: agent.id,
      environment_id: executionEnvironment.id,
      title: 'Before storage upgrade',
      betas: [...BETAS],
    });
    const deployment = await client.beta.deployments.create({
      agent: agent.id,
      environment_id: executionEnvironment.id,
      name: 'Before storage upgrade',
      initial_events: [{
        type: 'user.message',
        content: [{ type: 'text', text: 'Prepare the verified brief.' }],
      }],
      betas: [...BETAS],
    });
    await stopServer(server);
    server = null;

    const sessionRow = sqliteRows(
      database,
      'SELECT aggregate_json FROM managed_session WHERE session_id = ?',
      session.id,
    )[0] as { aggregate_json: string };
    const envelope = JSON.parse(sessionRow.aggregate_json);
    assert.equal(envelope.format, 'awaken.session.v1', 'fixture starts from current explicit format');
    const legacySession = envelope.aggregate;
    delete legacySession.event_batches;
    delete legacySession.active_activity_epochs;
    sqliteRun(
      database,
      'UPDATE managed_session SET aggregate_json = ? WHERE session_id = ?',
      JSON.stringify(legacySession),
      session.id,
    );
    sqliteRun(
      database,
      'INSERT INTO managed_session_quarantine (session_id, reason) VALUES (?, ?)',
      session.id,
      'missing field `event_batches` from the previous release',
    );

    const deploymentRow = sqliteRows(
      database,
      'SELECT data FROM managed_deployment WHERE deployment_id = ?',
      deployment.id,
    )[0] as { data: string };
    const legacyDeployment = JSON.parse(deploymentRow.data);
    assert.equal(legacyDeployment.initial_events[0].type, 'user_message');
    legacyDeployment.initial_events[0].type = 'user.message';
    sqliteRun(
      database,
      'UPDATE managed_deployment SET data = ? WHERE deployment_id = ?',
      JSON.stringify(legacyDeployment),
      deployment.id,
    );

    const reader = spawnServer('management', PORT, environment);
    server = reader.server;
    await waitForPort(PORT, 900_000, server);
    client = sdk();
    const restoredSession = await client.beta.sessions.retrieve(session.id, { betas: [...BETAS] });
    assert.equal(restoredSession.title, 'Before storage upgrade', 'U2');
    const restoredDeployment = await client.beta.deployments.retrieve(deployment.id, {
      betas: [...BETAS],
    });
    assert.equal(restoredDeployment.initial_events[0]?.type, 'user.message', 'U1');
    assert.equal(
      Number(sqliteRows(
        database,
        'SELECT COUNT(*) AS count FROM managed_session_quarantine WHERE session_id = ?',
        session.id,
      )[0]?.count),
      0,
      'U2 known historical shape is migrated rather than quarantined',
    );
    pass('U1/U2 old Deployment and Session storage remain official-SDK readable after restart');

    await client.beta.sessions.update(session.id, {
      title: 'After storage upgrade',
      betas: [...BETAS],
    });
    await client.beta.deployments.update(deployment.id, {
      name: 'After storage upgrade',
      betas: [...BETAS],
    });

    const leaked = await fetch(`http://127.0.0.1:${PORT}/v1/deployments`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: 'Bearer e2e-dummy' },
      body: JSON.stringify({
        agent: agent.id,
        environment_id: executionEnvironment.id,
        name: 'internal spelling must fail',
        initial_events: [{
          type: 'user_message',
          content: [{ type: 'text', text: 'must not persist' }],
        }],
      }),
    });
    assert.equal(leaked.status, 400, 'U5');
    await stopServer(server);
    server = null;

    const rewrittenSession = JSON.parse((sqliteRows(
      database,
      'SELECT aggregate_json FROM managed_session WHERE session_id = ?',
      session.id,
    )[0] as { aggregate_json: string }).aggregate_json);
    assert.equal(rewrittenSession.format, 'awaken.session.v1', 'U3');
    assert.ok(Array.isArray(rewrittenSession.aggregate.event_batches), 'U3');
    assert.ok(Array.isArray(rewrittenSession.aggregate.active_activity_epochs), 'U3');
    const rewrittenDeployment = JSON.parse((sqliteRows(
      database,
      'SELECT data FROM managed_deployment WHERE deployment_id = ?',
      deployment.id,
    )[0] as { data: string }).data);
    assert.equal(rewrittenDeployment.initial_events[0].type, 'user_message', 'U3');

    const finalProcess = spawnServer('management', PORT, environment);
    server = finalProcess.server;
    await waitForPort(PORT, 900_000, server);
    client = sdk();
    assert.equal(
      (await client.beta.sessions.retrieve(session.id, { betas: [...BETAS] })).title,
      'After storage upgrade',
      'U4',
    );
    assert.equal(
      (await client.beta.deployments.retrieve(deployment.id, { betas: [...BETAS] }))
        .initial_events[0]?.type,
      'user.message',
      'U4',
    );
    pass('U3/U4 current rewrite and a second restart preserve the public Managed contract');
    console.log('E2E PASS: Managed API and storage-upgrade compatibility are independently closed.');
  } finally {
    if (server) await stopServer(server);
    upstream.close();
    fs.rmSync(dataDir, { recursive: true, force: true });
    cleanupFixtureTree(sandboxDir);
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
