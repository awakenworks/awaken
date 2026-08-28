// ADR-0066 dynamic MCP attachment E2E through the official Anthropic TypeScript SDK.
// The test drives the one public full-replacement command; there is no MCP CRUD API.
//
// Cause-effect graph:
//   C1 desired set differs -> durable generation transition -> E1 exact active set
//   C2 Idempotency-Key seen with same hash -> E2 replay, no effect/event
//   C3 Idempotency-Key seen with another hash -> E3 conflict, no effect
//   C4 If-Match differs from root revision -> E4 conflict, no effect
//   C5 desired set empty -> E5 drain/remove, empty projection
//   C6 new key but desired set already converged -> E6 receipt-only root CAS, no domain effect
//   C7 two names resolve to one canonical target -> E7 reject before effect
//   C8 name/target is empty or logical name repeats -> E8 reject before effect
//   C9 stage fails -> E9 desired config remains readable; previous Active still executes
//
// Decision table:
// | Rule | desired | key       | If-Match | Effect |
// | H1   | replace | new       | absent   | switch A -> B and call only B |
// | H2   | same    | H1 replay | absent   | same ETag, no event/I/O |
// | H3   | remove  | H1 reused | absent   | 409, B remains active |
// | H4   | remove  | new       | stale    | 409, B remains active |
// | H5   | empty   | new       | current  | drain B, project empty |
// | H6   | add A   | new       | current  | new A generation and call A |
// | H7   | same A  | new       | current  | receipt ETag only, no event/I/O |
// | H8   | duplicate target | new | current | 400, A remains active |
// | H9   | empty name | new | current | 400, A remains active |
// | H10  | empty target | new | current | 400, A remains active |
// | H11  | duplicate name | new | current | 400, A remains active |
// | H12  | replace | new | absent | stage 500, desired rejected server is readable, A executes |
// | H13  | retry failed desired | new | absent | new stage attempt, desired remains readable, A executes |

import assert from 'node:assert/strict';
import http from 'node:http';
// @ts-ignore -- shared JS HTTP teardown deliberately serves TypeScript scenarios.
import { closeHttpServer } from './http_server.mjs';
import Anthropic from '@anthropic-ai/sdk';
// @ts-ignore -- shared JS harness deliberately serves both JS and TS scenarios.
import { withScenarioServer, pass } from './harness.mjs';
// @ts-ignore -- shared JS fixture deliberately serves both JS and TS scenarios.
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';
import {
  alwaysAllowMcpAgent,
  type McpServer,
  replaceMcpServers,
  responseEtag,
  retrieveSessionWithEtag,
  sendManagedMessage,
} from './fixtures/managed_mcp_session.ts';

const BETAS = ['managed-agents-2026-04-01'];
const TOKEN_A = 'hot-swap-token-a'; // awaken-allow: secret
const TOKEN_B = 'hot-swap-token-b'; // awaken-allow: secret

type Event = { type: string; [key: string]: unknown };

async function events(client: Anthropic, sessionId: string): Promise<Event[]> {
  const result: Event[] = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    result.push(event as unknown as Event);
  }
  return result;
}

async function update(
  client: Anthropic,
  sessionId: string,
  servers: McpServer[],
  headers: Record<string, string>,
) {
  return replaceMcpServers(client, sessionId, servers, BETAS, headers);
}

async function startRejectingMcp(): Promise<{
  url: string;
  requests: () => number;
  close: () => Promise<void>;
}> {
  let requests = 0;
  const server = http.createServer((_request, response) => {
    requests += 1;
    response.writeHead(500, { 'content-type': 'text/plain' });
    response.end('intentional stage failure');
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  assert.ok(address && typeof address !== 'string');
  return {
    url: `http://127.0.0.1:${address.port}/`,
    requests: () => requests,
    close: () => closeHttpServer(server),
  };
}

async function main(): Promise<void> {
  const fixtureA = await startCalcFixture(TOKEN_A);
  const fixtureB = await startCalcFixture(TOKEN_B);
  const rejecting = await startRejectingMcp();
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
          agent: alwaysAllowMcpAgent('assistant', [serverA]),
          environment_id: 'env_local',
          vault_ids: [vault.id],
          betas: BETAS,
        })
        .withResponse();
      const sessionId = created.data.id;
      const originalEtag = responseEtag(created.response);
      await sendManagedMessage(client, sessionId, 'add 1 2', BETAS);
      assert.equal(fixtureA.calls.filter((call: any) => call.method === 'tools/call').length, 1);

      const beforeReplaceEvents = (await events(client, sessionId)).filter(
        (event) => event.type === 'session.updated',
      ).length;
      const replaced = await update(client, sessionId, [serverB], { 'Idempotency-Key': 'swap-to-b' });
      const replacedEtag = responseEtag(replaced.response);
      assert.notEqual(replacedEtag, originalEtag, 'H1 advances the root revision');
      assert.deepEqual(replaced.data.agent.mcp_servers, [serverB], 'H1 projects only B');
      await sendManagedMessage(client, sessionId, 'add 3 4', BETAS);
      assert.equal(fixtureB.calls.filter((call: any) => call.method === 'tools/call').length, 1);
      assert.equal(fixtureA.calls.filter((call: any) => call.method === 'tools/call').length, 1);
      pass('H1 replace switches the exact active generation from A to B');

      const beforeReplayIo = fixtureB.calls.length;
      const replay = await update(client, sessionId, [serverB], { 'Idempotency-Key': 'swap-to-b' });
      assert.equal(responseEtag(replay.response), replacedEtag, 'H2 replays the committed revision');
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

      // H5's `current` cause is a fresh authority read: turns after H1 advance
      // the Session root, so the mutation response ETag is intentionally stale.
      const currentBeforeRemove = await retrieveSessionWithEtag(client, sessionId, BETAS);
      const removed = await update(client, sessionId, [], {
        'Idempotency-Key': 'remove-b',
        'If-Match': currentBeforeRemove.etag,
      });
      const removedEtag = responseEtag(removed.response);
      assert.notEqual(removedEtag, currentBeforeRemove.etag, 'H5 advances the root revision');
      assert.deepEqual(removed.data.agent.mcp_servers, [], 'H5 projects the drained set');
      pass('H5 remove drains B and projects an empty active set');

      const currentBeforeAdd = await retrieveSessionWithEtag(client, sessionId, BETAS);
      const added = await update(client, sessionId, [serverA], {
        'Idempotency-Key': 'add-a-again',
        'If-Match': currentBeforeAdd.etag,
      });
      assert.deepEqual(added.data.agent.mcp_servers, [serverA], 'H6 projects A generation N+1');
      await sendManagedMessage(client, sessionId, 'add 8 1', BETAS);
      assert.equal(fixtureA.calls.filter((call: any) => call.method === 'tools/call').length, 2);
      assert.equal(fixtureB.calls.filter((call: any) => call.method === 'tools/call').length, 1);

      const addedEtag = responseEtag(added.response);
      const beforeConvergedIo = fixtureA.calls.length;
      const beforeConvergedEvents = (await events(client, sessionId)).filter(
        (event) => event.type === 'session.updated',
      ).length;
      const currentBeforeConverged = await retrieveSessionWithEtag(client, sessionId, BETAS);
      const converged = await update(client, sessionId, [serverA], {
        'Idempotency-Key': 'same-a-new-command',
        'If-Match': currentBeforeConverged.etag,
      });
      const convergedEtag = responseEtag(converged.response);
      assert.notEqual(convergedEtag, addedEtag, 'H7 atomically records the new command receipt');
      assert.equal(fixtureA.calls.length, beforeConvergedIo, 'H7 performs no MCP I/O');
      assert.equal(
        (await events(client, sessionId)).filter((event) => event.type === 'session.updated').length,
        beforeConvergedEvents,
        'H7 emits no update event',
      );
      pass('H7 an already-converged desired set records only its command receipt');

      const duplicateTarget = { name: 'calc-alias', type: 'url' as const, url: fixtureA.url };
      const currentBeforeInvalid = await retrieveSessionWithEtag(client, sessionId, BETAS);
      await assert.rejects(
        update(client, sessionId, [serverA, duplicateTarget], {
          'Idempotency-Key': 'duplicate-canonical-target',
          'If-Match': currentBeforeInvalid.etag,
        }),
        (error: any) => error?.status === 400,
        'H8 rejects two logical names for one canonical MCP target',
      );
      assert.deepEqual(
        (await client.beta.sessions.retrieve(sessionId, { betas: BETAS })).agent.mcp_servers,
        [serverA],
        'H8 leaves the converged aggregate visible',
      );
      assert.equal(fixtureA.calls.length, beforeConvergedIo, 'H8 performs no MCP I/O');
      pass('H8 duplicate canonical targets fail before aggregate or Runtime effects');

      const invalidDesiredSets: Array<[string, McpServer[]]> = [
        ['H9', [{ name: '   ', type: 'url', url: fixtureA.url }]],
        ['H10', [{ name: 'empty-target', type: 'url', url: '   ' }]],
        ['H11', [serverA, { name: 'calc', type: 'url', url: fixtureB.url }]],
      ];
      for (const [rule, desired] of invalidDesiredSets) {
        await assert.rejects(
          update(client, sessionId, desired, {
            'Idempotency-Key': `invalid-${rule.toLowerCase()}`,
            'If-Match': currentBeforeInvalid.etag,
          }),
          (error: any) => error?.status === 400,
          `${rule} rejects malformed desired MCP state`,
        );
      }
      assert.deepEqual(
        (await client.beta.sessions.retrieve(sessionId, { betas: BETAS })).agent.mcp_servers,
        [serverA],
        'H9-H11 preserve the exact active set',
      );
      assert.equal(fixtureA.calls.length, beforeConvergedIo, 'H9-H11 perform no MCP I/O');
      pass('H9-H11 malformed names and targets fail before aggregate or Runtime effects');

      const rejectedServer: McpServer = {
        name: 'calc',
        type: 'url',
        url: rejecting.url,
      };
      await assert.rejects(
        update(client, sessionId, [rejectedServer], {
          'Idempotency-Key': 'stage-fails-once',
        }),
        (error: any) => error?.status === 500,
        'H12 surfaces the external stage failure',
      );
      const firstFailedRequests = rejecting.requests();
      assert.ok(firstFailedRequests > 0, 'H12 reaches the rejecting MCP upstream');
      assert.deepEqual(
        (await client.beta.sessions.retrieve(sessionId, { betas: BETAS })).agent.mcp_servers,
        [rejectedServer],
        'H12 keeps the accepted desired config readable independently of activation',
      );
      await sendManagedMessage(client, sessionId, 'add 2 5', BETAS);
      assert.equal(
        fixtureA.calls.filter((call: any) => call.method === 'tools/call').length,
        3,
        'H12 keeps the previous Active generation executable',
      );
      assert.equal(
        rejecting.requests(),
        firstFailedRequests,
        'H12 never publishes the failed desired generation as an execution route',
      );
      pass('H12 exposes desired config while the failed replacement preserves Active A');

      await assert.rejects(
        update(client, sessionId, [rejectedServer], {
          'Idempotency-Key': 'retry-failed-stage',
        }),
        (error: any) => error?.status === 500,
        'H13 surfaces the retried external stage failure',
      );
      assert.ok(
        rejecting.requests() > firstFailedRequests,
        'H13 a new command retries external realization instead of treating Failed as converged',
      );
      assert.deepEqual(
        (await client.beta.sessions.retrieve(sessionId, { betas: BETAS })).agent.mcp_servers,
        [rejectedServer],
        'H13 keeps the accepted desired config readable after retry failure',
      );
      const requestsAfterRetry = rejecting.requests();
      await sendManagedMessage(client, sessionId, 'add 4 6', BETAS);
      assert.equal(
        fixtureA.calls.filter((call: any) => call.method === 'tools/call').length,
        4,
        'H13 keeps the previous Active generation executable after retry failure',
      );
      assert.equal(
        rejecting.requests(),
        requestsAfterRetry,
        'H13 never routes execution through either failed generation',
      );
      pass('H13 retry allocates a new failed generation while Active A keeps executing');

      const serialized = JSON.stringify({ added: added.data, events: await events(client, sessionId) });
      assert.ok(!serialized.includes(TOKEN_A) && !serialized.includes(TOKEN_B), 'state/events remain secret-free');
      pass('H6 add after removal allocates a new generation and calls only A');
    });
    console.log('E2E PASS: ADR-0066 MCP hot replacement decision table.');
  } finally {
    await fixtureA.close();
    await fixtureB.close();
    await rejecting.close();
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
