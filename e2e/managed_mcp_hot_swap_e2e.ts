// ADR-0066 dynamic MCP attachment E2E through the official Anthropic TypeScript SDK.
// The test drives the one public full-replacement command; there is no MCP CRUD API.
//
// Cause-effect graph:
//   C1 desired set differs -> durable generation transition -> E1 exact active set
//   C2 Idempotency-Key seen with same hash -> E2 replay, no effect/event
//   C3 Idempotency-Key seen with another hash -> E3 conflict, no effect
//   C4 If-Match differs from root revision -> E4 conflict, no effect
//   C5 desired set empty -> E5 drain/remove, empty projection
//
// Decision table:
// | Rule | desired | key       | If-Match | Effect |
// | H1   | replace | new       | absent   | switch A -> B and call only B |
// | H2   | same    | H1 replay | absent   | same ETag, no event/I/O |
// | H3   | remove  | H1 reused | absent   | 409, B remains active |
// | H4   | remove  | new       | stale    | 409, B remains active |
// | H5   | empty   | new       | current  | drain B, project empty |
// | H6   | add A   | new       | current  | new A generation and call A |

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
// @ts-ignore -- shared JS harness deliberately serves both JS and TS scenarios.
import { withScenarioServer, pass } from './harness.mjs';
// @ts-ignore -- shared JS fixture deliberately serves both JS and TS scenarios.
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const TOKEN_A = 'hot-swap-token-a'; // awaken-allow: secret
const TOKEN_B = 'hot-swap-token-b'; // awaken-allow: secret

type McpServer = { name: string; type: 'url'; url: string };
type Event = { type: string; [key: string]: unknown };

async function events(client: Anthropic, sessionId: string): Promise<Event[]> {
  const result: Event[] = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    result.push(event as unknown as Event);
  }
  return result;
}

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
  headers: Record<string, string>,
) {
  return client.beta.sessions
    .update(
      sessionId,
      { agent: { mcp_servers: servers }, betas: BETAS } as never,
      { headers },
    )
    .withResponse();
}

function etag(response: Response): string {
  const value = response.headers.get('etag');
  assert.ok(value, 'a successful Session mutation returns ETag');
  return value;
}

async function main(): Promise<void> {
  const fixtureA = await startCalcFixture(TOKEN_A);
  const fixtureB = await startCalcFixture(TOKEN_B);
  try {
    await withScenarioServer('management', 'mcp', 38207, async (baseUrl: string) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const vault = await client.beta.vaults.create({ display_name: 'hot swap', betas: BETAS });
      for (const [url, token] of [[fixtureA.url, TOKEN_A], [fixtureB.url, TOKEN_B]]) {
        await client.beta.vaults.credentials.create(vault.id, {
          auth: { type: 'static_bearer', mcp_server_url: url, token },
          betas: BETAS,
        });
      }

      const serverA: McpServer = { name: 'calc', type: 'url', url: fixtureA.url };
      const serverB: McpServer = { name: 'calc', type: 'url', url: fixtureB.url };
      const created = await client.beta.sessions
        .create({
          agent: 'assistant',
          mcp_servers: [serverA],
          vault_ids: [vault.id],
          betas: BETAS,
        } as never)
        .withResponse();
      const sessionId = created.data.id;
      const originalEtag = etag(created.response);
      await send(client, sessionId, 'add 1 2');
      assert.equal(fixtureA.calls.filter((call: any) => call.method === 'tools/call').length, 1);

      const beforeReplaceEvents = (await events(client, sessionId)).filter(
        (event) => event.type === 'session.updated',
      ).length;
      const replaced = await update(client, sessionId, [serverB], { 'Idempotency-Key': 'swap-to-b' });
      const replacedEtag = etag(replaced.response);
      assert.notEqual(replacedEtag, originalEtag, 'H1 advances the root revision');
      assert.deepEqual(replaced.data.agent.mcp_servers, [serverB], 'H1 projects only B');
      await send(client, sessionId, 'add 3 4');
      assert.equal(fixtureB.calls.filter((call: any) => call.method === 'tools/call').length, 1);
      assert.equal(fixtureA.calls.filter((call: any) => call.method === 'tools/call').length, 1);
      pass('H1 replace switches the exact active generation from A to B');

      const beforeReplayIo = fixtureB.calls.length;
      const replay = await update(client, sessionId, [serverB], { 'Idempotency-Key': 'swap-to-b' });
      assert.equal(etag(replay.response), replacedEtag, 'H2 replays the committed revision');
      assert.equal(fixtureB.calls.length, beforeReplayIo, 'H2 performs no MCP I/O');
      const afterReplayEvents = (await events(client, sessionId)).filter(
        (event) => event.type === 'session.updated',
      ).length;
      assert.equal(afterReplayEvents, beforeReplaceEvents + 1, 'H2 emits no duplicate event');
      pass('H2 same idempotency key and payload replays without effect');

      await assert.rejects(
        update(client, sessionId, [], { 'Idempotency-Key': 'swap-to-b' }),
        (error: any) => error?.status === 409,
        'H3 key reuse with another payload conflicts',
      );
      assert.deepEqual((await client.beta.sessions.retrieve(sessionId, { betas: BETAS })).agent.mcp_servers, [serverB]);

      await assert.rejects(
        update(client, sessionId, [], { 'Idempotency-Key': 'stale-remove', 'If-Match': originalEtag }),
        (error: any) => error?.status === 409,
        'H4 stale root revision conflicts',
      );
      assert.deepEqual((await client.beta.sessions.retrieve(sessionId, { betas: BETAS })).agent.mcp_servers, [serverB]);
      pass('H3/H4 conflicts leave B active and perform no downgrade');

      const removed = await update(client, sessionId, [], {
        'Idempotency-Key': 'remove-b',
        'If-Match': replacedEtag,
      });
      const removedEtag = etag(removed.response);
      assert.deepEqual(removed.data.agent.mcp_servers, [], 'H5 projects the drained set');
      pass('H5 remove drains B and projects an empty active set');

      const added = await update(client, sessionId, [serverA], {
        'Idempotency-Key': 'add-a-again',
        'If-Match': removedEtag,
      });
      assert.deepEqual(added.data.agent.mcp_servers, [serverA], 'H6 projects A generation N+1');
      await send(client, sessionId, 'add 8 1');
      assert.equal(fixtureA.calls.filter((call: any) => call.method === 'tools/call').length, 2);
      assert.equal(fixtureB.calls.filter((call: any) => call.method === 'tools/call').length, 1);

      const serialized = JSON.stringify({ added: added.data, events: await events(client, sessionId) });
      assert.ok(!serialized.includes(TOKEN_A) && !serialized.includes(TOKEN_B), 'state/events remain secret-free');
      pass('H6 add after removal allocates a new generation and calls only A');
    });
    console.log('E2E PASS: ADR-0066 MCP hot replacement decision table.');
  } finally {
    await fixtureA.close();
    await fixtureB.close();
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
