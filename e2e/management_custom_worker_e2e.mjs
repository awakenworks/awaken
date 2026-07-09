// A *custom* self-hosted worker, driven by the official Anthropic TS SDK.
//
// Unlike the out-of-the-box `EnvironmentWorker` helper, this is a worker you build
// yourself from the raw Environments Work endpoints (poll / ack / heartbeat / stop)
// with your own per-session handling — the "call the Work endpoints directly and
// implement your own worker" path from the docs. It exercises the fan-out a single
// worker sees: MANY sessions assigned to one self-hosted environment enqueue many
// `session` work items, and the worker drains them one lease at a time (the
// open-tier single-worker cap), running each session's tool call with its OWN
// per-session result, while observing queue depth/pending.
//
// Run: (from e2e/)  node management_custom_worker_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38294);

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

const replyText = (m) => (m.content ?? []).map((c) => c.text ?? '').join('');

async function events(client, sessionId) {
  return drain(client.beta.sessions.events.list(sessionId, { betas: BETAS }));
}

async function main() {
  const { server, baseUrl } = spawnServer('worker', PORT);
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const work = client.beta.environments.work;

    // A self-hosted environment, and THREE sessions assigned to it — each is
    // enqueued as its own `session` work item (plus the seeded healthcheck).
    const env = await client.beta.environments.create({
      name: 'self-hosted-fleet',
      config: { type: 'self_hosted' },
      betas: BETAS,
    });
    const sessions = [];
    for (let i = 0; i < 3; i += 1) {
      sessions.push(await client.beta.sessions.create({ agent: 'assistant', environment_id: env.id, betas: BETAS }));
    }
    const sessionIds = sessions.map((s) => s.id).sort();

    const stats0 = await work.stats(env.id, { betas: BETAS });
    assert.equal(stats0.depth, 4, `3 session work items + 1 healthcheck queued, got depth ${stats0.depth}`);
    assert.equal(stats0.pending, 0, 'nothing claimed yet');
    pass('3 sessions on one self-hosted environment -> 3 `session` work items enqueued (fan-out)');

    // -- The custom worker's drain loop -----------------------------------
    // Claim one item at a time; a healthcheck is drained, a session is run with a
    // per-session tool result, then stopped so the next item can be leased.
    const handled = new Set();
    let sawPending = false;
    for (let guard = 0; guard < 12 && handled.size < sessions.length; guard += 1) {
      const item = await work.poll(env.id, { betas: BETAS });
      assert.ok(item, 'poll leases the next queued item');
      assert.equal(item.state, 'active');

      // While this item is leased, the queue reports it as `pending` (in-flight).
      const midStats = await work.stats(env.id, { betas: BETAS });
      sawPending = sawPending || midStats.pending >= 1;

      if (item.data.type === 'healthcheck') {
        await work.stop(item.id, { environment_id: env.id, betas: BETAS });
        continue;
      }

      // A session work item: the worker owns the whole per-session lifecycle.
      assert.equal(item.data.type, 'session');
      const sessionId = item.data.id;
      await work.ack(item.id, { environment_id: env.id, betas: BETAS });
      await work.heartbeat(item.id, { environment_id: env.id, betas: BETAS });

      // Drive the session; it parks on the agent's client-executed tool call.
      await client.beta.sessions.events.send(sessionId, {
        betas: BETAS,
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'do the task' }] }],
      });
      const parked = (await events(client, sessionId)).find((e) => e.type === 'agent.custom_tool_use');
      assert.ok(parked, `session ${sessionId} parked on a tool call`);

      // The worker's OWN per-session tool logic: a result unique to this session,
      // proving the worker (not the server) computed it.
      const answer = `handled-${sessionId}`;
      await client.beta.sessions.events.send(sessionId, {
        betas: BETAS,
        events: [{ type: 'user.tool_result', tool_use_id: parked.id, content: [{ type: 'text', text: answer }] }],
      });
      const replies = (await events(client, sessionId)).filter((e) => e.type === 'agent.message');
      assert.ok(
        replies.some((m) => replyText(m).includes(answer)),
        `session ${sessionId}: the worker's per-session result reached the model`,
      );

      await work.stop(item.id, { environment_id: env.id, betas: BETAS });
      handled.add(sessionId);
    }

    assert.deepEqual([...handled].sort(), sessionIds, 'the worker ran every session assigned to the environment');
    assert.ok(sawPending, 'the queue reported a claimed item as pending while in flight');
    pass('custom worker drains all sessions (poll -> ack -> heartbeat -> run per-session tool -> stop)');

    // The queue is drained: nothing left to claim.
    const drained = await work.stats(env.id, { betas: BETAS });
    assert.equal(drained.depth, 0, `queue fully drained, got depth ${drained.depth}`);
    assert.equal(await work.poll(env.id, { betas: BETAS }), null, 'poll returns null on an empty queue');
    pass('the environment queue is fully drained (depth 0, poll -> null)');

    console.log('E2E PASS: a custom worker drains a fan-out of sessions on one self-hosted environment.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server);
  }
}

main();
