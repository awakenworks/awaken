// The self-hosted worker data plane, driven by the official Anthropic TS SDK.
//
// Faithful to the Managed Agents self-hosted model (docs: "the self_hosted
// environment acts as a work queue: when a session is assigned to it, Anthropic
// enqueues the session as a work item"): a session created on a self-hosted
// environment is dispatched as a `session` work item. This e2e IS a worker — it
// polls the environment's queue, claims the session work, drives the session,
// and — the way a self-hosted worker executes the session's tool calls — RUNS the
// agent's awaiting `submit_answer` tool and posts the result back. Full lifecycle:
// poll -> ack -> heartbeat -> run tool call -> stop, over real HTTP.
//
// Run: (from e2e/)  node management_self_hosted_worker_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  pass,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38290);

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  // Test design (self-hosted worker lifecycle). Causes: C1=a Session is assigned
  // to one self-hosted Environment; C2=the worker polls/acks/heartbeats its lease;
  // C3=the Session parks on a qualified custom tool; C4=the worker posts the
  // matching result and stops work. Effects: E1=healthcheck+Session enqueue;
  // E2=one active lease; E3=the exact Run resumes/ends; E4=later Session Events
  // do not enqueue duplicate Session work. Constraints/invariant: queue lease
  // custody and public Event id are independent single authorities.
  // Decision rules: H1=C1=>E1; H2=C1+C2=>E2; H3=H2+C3+C4=>E3+E4.
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
          // Drain the seeded healthcheck so the next poll reaches the session work,
          // exercising the `force` stop variant (SDK WorkStopParams.force) end-to-end:
          // the managed API accepts `force: true` and stops the item. (The graceful-
          // vs-force distinction — immediate vs drain — is a worker-executor concern;
          // the server contract asserted here is that the force parameter round-trips
          // and the item leaves the active lease.)
          const stopped = await client.beta.environments.work.stop(work.id, {
            environment_id: env.id,
            force: true,
            betas: BETAS,
          });
          assert.equal(stopped.id, work.id, 'force-stop is accepted and returns the stopped item');
          assert.notEqual(stopped.state, 'active', 'force-stop moves the item off the active lease');
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
        // signal (create / dormant wake), so the Session Events below must NOT add new
        // work items (that is dispatch's job, not the work queue's).
        const beforeDrive = await drain(client.beta.environments.work.list(env.id, { betas: BETAS }));
        const sessionWorkBefore = beforeDrive.filter((w) => w.data.type === 'session').length;

        const hb = await client.beta.environments.work.heartbeat(work.id, {
          environment_id: env.id,
          betas: BETAS,
        });
        assert.equal(hb.lease_extended, true, 'heartbeat extends the lease');

        // Cause/effect graph: C1 the claimed Session receives a User Event; C2
        // `submit_answer` is declared client-executed and therefore projects an
        // `agent.custom_tool_use`; C3 the worker answers the exact public Event
        // id with its matching custom-result family. Effects: E1 the Run parks at
        // requires_action; E2 one result resumes it; E3 one terminal Agent Message
        // contains 42; E4 no additional environment work is enqueued.
        // Decision table: W1=C1+C2 -> E1; W2=C1+C2+C3 -> E2+E3+E4. A generic
        // `user.tool_result` is excluded because it answers only `agent.tool_use`.
        const taskReceipt = await client.beta.sessions.events.send(session.id, {
          betas: BETAS,
          events: [{ type: 'user.message', content: [{ type: 'text', text: 'run the task' }] }],
        });
        const { delta: events } = await waitForSessionEventReceipt(
          client,
          session.id,
          taskReceipt.data[0]?.id,
          BETAS,
          ({ delta }) => delta.some((event) => event.type === 'agent.custom_tool_use')
            && delta.some((event) =>
              event.type === 'session.status_idle'
                && event.stop_reason?.type === 'requires_action'),
          'W1 custom tool reaches its durable requires_action boundary',
        );
        const toolUse = events.find((e) => e.type === 'agent.custom_tool_use');
        assert.ok(toolUse, `session awaiting on a tool call, got ${events.map((e) => e.type)}`);
        const idle = events.find((e) => e.type === 'session.status_idle');
        assert.equal(idle.stop_reason.type, 'requires_action', 'the session awaits the worker to run the tool');

        // The worker RUNS the tool call locally and posts the result back.
        const resultReceipt = await client.beta.sessions.events.send(session.id, {
          betas: BETAS,
          events: [
            { type: 'user.custom_tool_result', custom_tool_use_id: toolUse.id, content: [{ type: 'text', text: '42' }] },
          ],
        });
        const { delta } = await waitForSessionEventReceipt(
          client,
          session.id,
          resultReceipt.data[0]?.id,
          BETAS,
          ({ delta: current }) => current.some((message) => message.type === 'agent.message'
            && (message.content ?? []).some((content) => (content.text ?? '').includes('42'))),
          'W2 exact custom result reaches the terminal Agent Message',
        );
        const replies = delta.filter((event) => event.type === 'agent.message');
        assert.ok(
          replies.some((m) => (m.content ?? []).some((c) => (c.text ?? '').includes('42'))),
          `the worker's tool result reached the model, replies: ${JSON.stringify(replies)}`,
        );
        pass('worker runs the awaiting tool call and posts the result back');

        // The two Events above (user.message + user.custom_tool_result) drove the Session
        // through the events API — they must NOT have enqueued any new work: the queue
        // still holds exactly the one `session` work item it had before the Run.
        const afterDrive = await drain(client.beta.environments.work.list(env.id, { betas: BETAS }));
        const sessionWorkAfter = afterDrive.filter((w) => w.data.type === 'session').length;
        assert.equal(
          sessionWorkAfter,
          sessionWorkBefore,
          'driving the session with messages must not enqueue new work (work != dispatch)',
        );
        pass('Session Events do not re-enqueue work (work is a per-Session-creation signal)');

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
