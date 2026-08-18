// Official TypeScript SessionToolRunner fault/reconnect matrix.
//
// Cause/effect graph: durable/listed tool-use + live SSE delivery + local
// registry/result -> one dispatch and at most one matching durable result.
// Decision table: owned success/error are posted once; unowned tools are left
// pending; an SSE cut reconciles through list without duplicate execution.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { betaZodTool } from '@anthropic-ai/sdk/helpers/beta/zod';
import * as z from 'zod';
import { pass, spawnServer, stopServer, waitForPort } from './harness.mjs';

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
  await client.beta.sessions.events.send(sessionID, {
    events: [{
      type: 'user.message',
      content: [{ type: 'text', text: 'answer it' }],
    }],
    betas: BETAS,
  });
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

function cutFirstEventStream() {
  let cut = false;
  return async (input, init) => {
    const response = await fetch(input, init);
    const url = typeof input === 'string' ? input : input.url;
    if (cut || !url.includes('/events/stream') || !response.body) return response;
    cut = true;
    const reader = response.body.getReader();
    let delivered = false;
    const body = new ReadableStream({
      async pull(controller) {
        if (!delivered) {
          const next = await reader.read();
          if (next.done) {
            controller.close();
            return;
          }
          delivered = true;
          controller.enqueue(next.value);
          return;
        }
        await reader.cancel('injected SSE disconnect');
        controller.error(new Error('injected SSE disconnect'));
      },
      cancel(reason) {
        return reader.cancel(reason);
      },
    });
    return new Response(body, {
      status: response.status,
      statusText: response.statusText,
      headers: response.headers,
    });
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

  // STR-03: cut the first SSE response. SessionToolRunner reconnects and
  // reconciles against events.list; the durable tool use is dispatched once.
  {
    const session = await createSession(client, environment.id, 'tool-reconnect');
    await sendTask(client, session.id);
    let runs = 0;
    const reconnecting = new Anthropic({
      apiKey: 'e2e-dummy',
      baseURL: baseUrl,
      fetch: cutFirstEventStream(),
    });
    const calls = [];
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
