// Official TypeScript SessionToolRunner fault/reconnect matrix.
//
// Cause/effect graph: durable/listed tool-use + live SSE delivery + local
// registry/result -> one dispatch and at most one matching durable result.
// Decision table: owned success/error are posted once; unowned tools are left
// pending; an SSE cut reconciles through list without duplicate execution.

import assert from 'node:assert/strict';
import http from 'node:http';
import Anthropic from '@anthropic-ai/sdk';
import { betaZodTool } from '@anthropic-ai/sdk/helpers/beta/zod';
import * as z from 'zod';
import {
  availablePort,
  pass,
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionCustomToolBoundary,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38343);

async function drain(page) {
  const rows = [];
  for await (const row of page) rows.push(row);
  return rows;
}

async function createSession(client, environmentID, name) {
  return client.beta.sessions.create({
    agent: 'assistant',
    environment_id: environmentID,
    title: name,
    betas: BETAS,
  });
}

async function sendTask(client, sessionID) {
  // C1=exact task receipt; C2=custom tool pending+requires_action. E1=C2 after
  // C1 makes the official runner's reconciliation input durable. K: this wait
  // never executes the client tool. Decision T1 C1&&!C2=>retry; T2=>start runner.
  const receipt = await client.beta.sessions.events.send(sessionID, {
    events: [{
      type: 'user.message',
      content: [{ type: 'text', text: 'answer it' }],
    }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'T1 exact SessionToolRunner task receipt');
  await waitForSessionCustomToolBoundary(
    client,
    sessionID,
    receiptId,
    BETAS,
    'submit_answer',
    'T1 custom tool task to commit before SessionToolRunner starts',
  );
}

function tool(run, close = undefined) {
  return betaZodTool({
    name: 'submit_answer',
    description: 'Answer the deterministic fixture.',
    inputSchema: z.object({ question: z.string() }),
    run,
    ...(close ? { close } : {}),
  });
}

async function startCutOnceProxy(targetBaseURL) {
  const target = new URL(targetBaseURL);
  const sockets = new Set();
  let selectedForCut = false;
  let cuts = 0;
  const proxy = http.createServer((incoming, outgoing) => {
    const url = new URL(incoming.url, target);
    const headers = { ...incoming.headers, host: url.host };
    const upstream = http.request(url, { method: incoming.method, headers }, (response) => {
      outgoing.writeHead(response.statusCode, response.statusMessage, response.headers);
      if (!selectedForCut && url.pathname.endsWith('/events/stream')) {
        selectedForCut = true;
        response.once('data', (chunk) => {
          // Flush actual SSE bytes over TCP and then sever the socket. The next
          // SDK request gets a fresh proxy connection and reaches the server.
          outgoing.write(chunk, () => {
            cuts += 1;
            outgoing.socket?.destroy();
          });
          response.destroy();
        });
        response.once('end', () => {
          if (!outgoing.destroyed) outgoing.end();
        });
      } else {
        response.pipe(outgoing);
      }
    });
    upstream.on('error', (error) => {
      if (!outgoing.destroyed) outgoing.destroy(error);
    });
    incoming.pipe(upstream);
  });
  proxy.on('connection', (socket) => {
    sockets.add(socket);
    socket.once('close', () => sockets.delete(socket));
  });
  const port = await availablePort(PORT + 1);
  await new Promise((resolve, reject) => {
    proxy.once('error', reject);
    proxy.listen(port, '127.0.0.1', resolve);
  });
  return {
    baseURL: `http://127.0.0.1:${port}`,
    cuts: () => cuts,
    close: () => new Promise((resolve, reject) => {
      for (const socket of sockets) socket.destroy();
      proxy.close((error) => (error ? reject(error) : resolve()));
    }),
  };
}

const { server, baseUrl } = spawnServer('worker', PORT);
try {
  await waitForPort(PORT);
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
  const environment = await client.beta.environments.create({
    name: 'tool-runner-matrix',
    config: { type: 'self_hosted' },
    betas: BETAS,
  });

  // STR-01: a thrown custom tool is represented as an error result; it does
  // not crash the iterator or leave the Session permanently requires_action.
  {
    const session = await createSession(client, environment.id, 'tool-error');
    await sendTask(client, session.id);
    let closes = 0;
    const calls = [];
    for await (const call of client.beta.sessions.events.toolRunner(session.id, {
      tools: [tool(async () => {
        throw new Error('deterministic tool failure');
      }, async () => {
        closes += 1;
      })],
      betas: BETAS,
      maxIdleMs: 100,
      signal: AbortSignal.timeout(15_000),
    })) {
      calls.push(call);
    }
    assert.equal(calls.length, 1);
    assert.equal(calls[0].name, 'submit_answer');
    assert.equal(calls[0].isError, true);
    assert.equal(calls[0].posted, true);
    assert.equal(calls[0].result?.type, 'user.custom_tool_result');
    assert.equal(closes, 1);
    const events = await drain(client.beta.sessions.events.list(session.id, { betas: BETAS }));
    assert.equal(events.filter((event) => event.type === 'user.custom_tool_result').length, 1);
    pass('STR-01 thrown custom tools post one typed error result and close cleanly');
  }

  // STR-02: split-client ownership. An unknown local tool is observed but not
  // answered, so another client can own it without receiving a synthetic error.
  {
    const session = await createSession(client, environment.id, 'tool-unowned');
    await sendTask(client, session.id);
    const controller = new AbortController();
    let observed;
    for await (const call of client.beta.sessions.events.toolRunner(session.id, {
      tools: [],
      betas: BETAS,
      maxIdleMs: 0,
      signal: controller.signal,
    })) {
      observed = call;
      controller.abort();
    }
    assert.equal(observed?.name, 'submit_answer');
    assert.equal(observed?.posted, false);
    assert.equal(observed?.result, undefined);
    const events = await drain(client.beta.sessions.events.list(session.id, { betas: BETAS }));
    assert.equal(events.filter((event) => event.type === 'user.custom_tool_result').length, 0);
    assert.ok(events.some((event) =>
      event.type === 'session.status_idle' && event.stop_reason?.type === 'requires_action'));
    pass('STR-02 unowned tools stay pending without a fabricated result');
  }

  // STR-03: a real HTTP proxy severs the first SSE TCP connection after bytes
  // arrive. SessionToolRunner reconnects and reconciles against events.list;
  // the durable tool use is dispatched once. This is a transport failure, not
  // an in-process ReadableStream/fetch mock.
  {
    const session = await createSession(client, environment.id, 'tool-reconnect');
    await sendTask(client, session.id);
    let runs = 0;
    const cuttingProxy = await startCutOnceProxy(baseUrl);
    const calls = [];
    try {
      const reconnecting = new Anthropic({
        apiKey: 'e2e-dummy',
        baseURL: cuttingProxy.baseURL,
      });
      for await (const call of reconnecting.beta.sessions.events.toolRunner(session.id, {
        tools: [tool(async () => {
          runs += 1;
          return '42';
        })],
        betas: BETAS,
        maxIdleMs: 100,
        signal: AbortSignal.timeout(20_000),
      })) {
        calls.push(call);
      }
      assert.equal(cuttingProxy.cuts(), 1, 'one physical SSE connection was severed');
    } finally {
      await cuttingProxy.close();
    }
    const events = await drain(client.beta.sessions.events.list(session.id, { betas: BETAS }));
    const uses = events.filter((event) => event.type === 'agent.custom_tool_use');
    const results = events.filter((event) => event.type === 'user.custom_tool_result');
    assert.equal(runs, 1);
    assert.equal(calls.length, 1);
    assert.equal(results.length, 1);
    assert.equal(results[0].custom_tool_use_id, uses[0].id);
    pass('STR-03 SSE disconnect reconciles without duplicate tool execution/result');
  }

  // STR-04: the helper is deliberately a single-consumer async iterable.
  {
    const session = await createSession(client, environment.id, 'tool-consumed');
    const runner = client.beta.sessions.events.toolRunner(session.id, {
      tools: [],
      betas: BETAS,
      maxIdleMs: 50,
      signal: AbortSignal.timeout(2_000),
    });
    for await (const _ of runner) {}
    await assert.rejects(async () => {
      for await (const _ of runner) {}
    }, /consumed SessionToolRunner/);
    pass('STR-04 consumed SessionToolRunner fails deterministically');
  }

  console.log('E2E PASS: official TypeScript SessionToolRunner error/ownership/reconnect matrix.');
} finally {
  await stopServer(server);
}
