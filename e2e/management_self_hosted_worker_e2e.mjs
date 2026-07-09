// The self-hosted worker data plane, driven by the official Anthropic TS SDK.
//
// Faithful to the Managed Agents self-hosted model (docs: "the self_hosted
// environment acts as a work queue: when a session is assigned to it, Anthropic
// enqueues the session as a work item"): a session created on a self-hosted
// environment is dispatched as a `session` work item. This e2e IS a worker — it
// polls the environment's queue, claims the session work, drives the session
// (send a message, read the agent reply), heartbeats the lease, and stops the
// work — the full poll -> ack -> heartbeat -> stop lifecycle over real HTTP.
//
// Run: (from e2e/)  node management_self_hosted_worker_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function agentReplies(client, sessionId) {
  const evs = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) evs.push(ev);
  return evs.filter((e) => e.type === 'agent.message');
}

async function main() {
  try {
    await withScenarioServer('worker', 'echo', 38290, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // A self-hosted environment: sessions on it dispatch through its work queue.
      const env = await client.beta.environments.create({
        name: 'self-hosted-prod',
        config: { type: 'self_hosted' },
        betas: BETAS,
      });
      assert.equal(env.config.type, 'self_hosted');

      // Assigning a session to the environment enqueues it as `session` work,
      // alongside the environment's seeded healthcheck.
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: env.id,
        betas: BETAS,
      });
      const queued = await drain(client.beta.environments.work.list(env.id, { betas: BETAS }));
      const kinds = queued.map((w) => w.data.type).sort();
      assert.deepEqual(kinds, ['healthcheck', 'session'], `queue has healthcheck + session, got ${kinds}`);
      const sessionWork = queued.find((w) => w.data.type === 'session');
      assert.equal(sessionWork.data.id, session.id, 'the session work references the session id');
      pass('a session on a self-hosted environment is enqueued as `session` work');

      // -- Act as the worker: poll the queue and handle each item ------------
      // Single active lease per environment (the open-tier single-worker cap), so
      // claim → handle → stop one at a time until the session work is done.
      let handledSession = false;
      for (let i = 0; i < 5 && !handledSession; i += 1) {
        const work = await client.beta.environments.work.poll(env.id, { betas: BETAS });
        assert.ok(work, 'poll leases a queued item');
        assert.equal(work.state, 'active', 'a claimed item is active');

        if (work.data.type === 'healthcheck') {
          // Drain the seeded healthcheck so the next poll reaches the session work.
          await client.beta.environments.work.stop(work.id, { environment_id: env.id, betas: BETAS });
          continue;
        }

        // The session work: this is the real payload. Claim it and run the session.
        assert.equal(work.data.type, 'session');
        assert.equal(work.data.id, session.id);
        const acked = await client.beta.environments.work.ack(work.id, {
          environment_id: env.id,
          betas: BETAS,
        });
        assert.ok(acked.acknowledged_at, 'the worker acked the session work');

        const hb = await client.beta.environments.work.heartbeat(work.id, {
          environment_id: env.id,
          betas: BETAS,
        });
        assert.equal(hb.lease_extended, true, 'heartbeat extends the lease');

        // Drive the session the worker just claimed: send a turn, read the reply.
        await client.beta.sessions.events.send(session.id, {
          betas: BETAS,
          events: [{ type: 'user.message', content: [{ type: 'text', text: 'hello from the worker' }] }],
        });
        const replies = await agentReplies(client, session.id);
        assert.ok(replies.length >= 1, `the worker-driven session produced a reply, got ${replies.length}`);

        // Finish: stop the work item (the worker releases the session).
        const stopped = await client.beta.environments.work.stop(work.id, {
          environment_id: env.id,
          betas: BETAS,
        });
        assert.equal(stopped.state, 'stopped', 'the worker stopped the session work');
        handledSession = true;
      }
      assert.ok(handledSession, 'the worker claimed and ran the session work');
      pass('worker poll -> ack -> heartbeat -> drive session -> stop (full lifecycle)');
    });

    console.log('E2E PASS: self-hosted worker claims a session from the work queue and runs it.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
