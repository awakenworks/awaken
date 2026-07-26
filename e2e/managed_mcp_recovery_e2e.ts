// ADR-0066 durable MCP generation recovery through a real process restart.
//
// Cause-effect graph:
//   C1 durable generation Active + owner lost -> new lease/claim -> E1 restage and republish exact generation
//   C2 exact idempotency receipt persisted -> retry after restart -> E2 replay without new effect
//   C3 durable generation Removed + restart -> E3 never republish
//
// Decision table:
// | Rule | durable state | trigger | Effect |
// | R0 | idle baseline, no transcript | restart/retrieve | reconstruct idle Session |
// | R1 | Active B | restart | same Session calls only B |
// | R2 | Active B + update receipt | replay key after restart | same ETag, no MCP I/O/event |
// | R3 | Removed B | restart | empty projection, no B resurrection |

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
// @ts-ignore -- shared JS harness deliberately serves TS restart scenarios.
import { availablePort, pass, realServerEnv, spawnServer, startUpstream, stopServer, waitForPort } from './harness.mjs';
// @ts-ignore -- shared JS fixture deliberately serves TS scenarios.
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PREFERRED_PORT = 38208;
const TOKEN_A = 'recovery-token-a'; // awaken-allow: secret
const TOKEN_B = 'recovery-token-b'; // awaken-allow: secret

type McpServer = { name: string; type: 'url'; url: string };

async function send(client: Anthropic, sessionId: string, text: string): Promise<void> {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

async function update(
  client: Anthropic,
  sessionId: string,
  servers: McpServer[],
  key: string,
  ifMatch?: string,
) {
  return client.beta.sessions
    .update(
      sessionId,
      { agent: { mcp_servers: servers }, betas: BETAS } as never,
      {
        headers: {
          'Idempotency-Key': key,
          ...(ifMatch === undefined ? {} : { 'If-Match': ifMatch }),
        },
      },
    )
    .withResponse();
}

function etag(response: Response): string {
  const value = response.headers.get('etag');
  assert.ok(value, 'Session response has an ETag');
  return value;
}

async function main(): Promise<void> {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-mcp-recovery-'));
  const port = await availablePort(PREFERRED_PORT);
  const fixtureA = await startCalcFixture(TOKEN_A);
  const fixtureB = await startCalcFixture(TOKEN_B);
  const upstream = await startUpstream('mcp');
  const env = {
    SESSION_DEPLOYMENT_STORAGE_DIR: path.join(root, 'runtime'),
    ...realServerEnv('mcp', upstream, { mode: 'management' }),
  };
  let server: any = null;
  try {
    let started = spawnServer('management', port, env);
    server = started.server;
    let baseUrl = started.baseUrl;
    await waitForPort(port, 180_000, server);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const vault = await client.beta.vaults.create({ display_name: 'recovery', betas: BETAS });
    for (const [url, token] of [[fixtureA.url, TOKEN_A], [fixtureB.url, TOKEN_B]]) {
      await client.beta.vaults.credentials.create(vault.id, {
        auth: { type: 'static_bearer', mcp_server_url: url, token },
        betas: BETAS,
      });
    }
    const serverA: McpServer = { name: 'calc', type: 'url', url: fixtureA.url };
    const serverB: McpServer = { name: 'calc', type: 'url', url: fixtureB.url };
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      mcp_servers: [serverA],
      vault_ids: [vault.id],
      betas: BETAS,
    } as never);
    const emptySession = await client.beta.sessions.create({
      agent: 'assistant',
      betas: BETAS,
    });
    const replaced = await update(client, session.id, [serverB], 'persisted-swap-b');
    const replaceEtag = etag(replaced.response);
    await send(client, session.id, 'add 2 5');
    assert.equal(fixtureB.calls.filter((call: any) => call.method === 'tools/call').length, 1);

    await stopServer(server);
    server = null;
    started = spawnServer('management', port, env);
    server = started.server;
    baseUrl = started.baseUrl;
    await waitForPort(port, 180_000, server);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const recoveredEmpty = await client.beta.sessions.retrieve(emptySession.id, { betas: BETAS });
    assert.equal(recoveredEmpty.status, 'idle', 'R0 reconstructs a transcript-free Session');
    pass('R0 durable idle Session survives restart without requiring transcript rows');
    const recovered = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.deepEqual(recovered.agent.mcp_servers, [serverB], 'R1 restores durable active B');
    await send(client, session.id, 'add 6 7');
    assert.equal(fixtureB.calls.filter((call: any) => call.method === 'tools/call').length, 2);
    assert.equal(fixtureA.calls.filter((call: any) => call.method === 'tools/call').length, 0);
    pass('R1 restart reacquires ownership and calls only exact active B');

    const beforeReplayIo = fixtureB.calls.length;
    const replay = await update(client, session.id, [serverB], 'persisted-swap-b');
    assert.equal(etag(replay.response), replaceEtag, 'R2 returns the committed revision');
    assert.equal(fixtureB.calls.length, beforeReplayIo, 'R2 performs no realization I/O');
    pass('R2 idempotency receipt survives restart and replays without effect');

    const current = await client.beta.sessions
      .retrieve(session.id, { betas: BETAS })
      .withResponse();
    const removed = await update(
      client,
      session.id,
      [],
      'persisted-remove-b',
      etag(current.response),
    );
    assert.deepEqual(removed.data.agent.mcp_servers, []);
    await stopServer(server);
    server = null;
    started = spawnServer('management', port, env);
    server = started.server;
    baseUrl = started.baseUrl;
    await waitForPort(port, 180_000, server);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const afterRemovalRestart = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.deepEqual(afterRemovalRestart.agent.mcp_servers, [], 'R3 removed B stays removed');
    assert.equal(fixtureB.calls.filter((call: any) => call.method === 'tools/call').length, 2);
    pass('R3 restart never republishes a removed generation');

    const serialized = JSON.stringify(afterRemovalRestart);
    assert.ok(!serialized.includes(TOKEN_A) && !serialized.includes(TOKEN_B));
    console.log('E2E PASS: ADR-0066 MCP recovery decision table.');
  } finally {
    if (server) await stopServer(server);
    await fixtureA.close();
    await fixtureB.close();
    upstream.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
