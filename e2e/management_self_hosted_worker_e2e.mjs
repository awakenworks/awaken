// The self-hosted worker data plane, driven by the official Anthropic TS SDK.
//
// Faithful to the Managed Agents self-hosted model (docs: "the self_hosted
// environment acts as a work queue: when a session is assigned to it, Anthropic
// enqueues the session as a work item"): a session created on a self-hosted
// environment is dispatched as a `session` work item. This e2e IS a worker — it
// polls the environment's queue, claims the session work, drives the session,
// and — the way a self-hosted worker executes the session's tool calls — RUNS the
// agent's parked `submit_answer` tool and posts the result back. Full lifecycle:
// poll -> ack -> heartbeat -> run tool call -> stop, over real HTTP.
//
// Run: (from e2e/)  node management_self_hosted_worker_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38290);

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
  const { server, baseUrl } = spawnServer('worker', PORT);
  try {
    await waitForPort(PORT);
    {
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

        // Snapshot the queue before driving the session: work is a session-lifecycle
        // signal (create / dormant wake), so the message turns below must NOT add new
        // work items (that is dispatch's job, not the work queue's).
        const beforeDrive = await drain(client.beta.environments.work.list(env.id, { betas: BETAS }));
        const sessionWorkBefore = beforeDrive.filter((w) => w.data.type === 'session').length;

        const hb = await client.beta.environments.work.heartbeat(work.id, {
          environment_id: env.id,
          betas: BETAS,
        });
        assert.equal(hb.lease_extended, true, 'heartbeat extends the lease');

        // Drive the session the worker just claimed: a turn parks on the agent's
        // client-executed `submit_answer` tool call.
        await client.beta.sessions.events.send(session.id, {
          betas: BETAS,
          events: [{ type: 'user.message', content: [{ type: 'text', text: 'run the task' }] }],
        });
        const events = await drain(client.beta.sessions.events.list(session.id, { betas: BETAS }));
        const toolUse = events.find((e) => e.type === 'agent.custom_tool_use');
        assert.ok(toolUse, `session parked on a tool call, got ${events.map((e) => e.type)}`);
        const idle = events.find((e) => e.type === 'session.status_idle');
        assert.equal(idle.stop_reason.type, 'requires_action', 'the session awaits the worker to run the tool');

        // The worker RUNS the tool call locally and posts the result back.
        await client.beta.sessions.events.send(session.id, {
          betas: BETAS,
          events: [
            { type: 'user.tool_result', tool_use_id: toolUse.id, content: [{ type: 'text', text: '42' }] },
          ],
        });
        const replies = await agentReplies(client, session.id);
        assert.ok(
          replies.some((m) => (m.content ?? []).some((c) => (c.text ?? '').includes('42'))),
          `the worker's tool result reached the model, replies: ${JSON.stringify(replies)}`,
        );
        pass('worker runs the parked tool call and posts the result back');

        // The two turns above (user.message + user.tool_result) drove the session
        // through the events API — they must NOT have enqueued any new work: the queue
        // still holds exactly the one `session` work item it had before the turns.
        const afterDrive = await drain(client.beta.environments.work.list(env.id, { betas: BETAS }));
        const sessionWorkAfter = afterDrive.filter((w) => w.data.type === 'session').length;
        assert.equal(
          sessionWorkAfter,
          sessionWorkBefore,
          'driving the session with messages must not enqueue new work (work != dispatch)',
        );
        pass('session message turns do not re-enqueue work (work is a per-session-creation signal)');

        // Finish: stop the work item (the worker releases the session).
        const stopped = await client.beta.environments.work.stop(work.id, {
          environment_id: env.id,
          betas: BETAS,
        });
        assert.equal(stopped.state, 'stopped', 'the worker stopped the session work');
        handledSession = true;
      }
      assert.ok(handledSession, 'the worker claimed and ran the session work');
      pass('worker poll -> ack -> heartbeat -> run tool -> stop (full lifecycle)');
    }

    console.log('E2E PASS: self-hosted worker claims a session, runs its tool calls, and stops the work.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server);
  }
}

main();
