// EnvironmentWorker differential: drive awaken's self-hosted work queue with the
// OFFICIAL SDK worker helper, not hand-rolled poll/ack/stop calls. The reference
// baseline the matrix said was "missing" ships inside @anthropic-ai/sdk as
// `client.beta.environments.work.poller` (WorkPoller) / `.worker` (EnvironmentWorker).
// If awaken's work-queue API is compatible with the official worker, the official
// poller claims, acks, yields, and force-stops awaken's queued items unmodified —
// which is exactly the "vs the official EnvironmentWorker" conformance check.
//
// This uses the control-plane WorkPoller (poll -> ack -> yield -> stop) as the
// reference; the full EnvironmentWorker.run() additionally downloads skills + runs
// the agent toolset in a workdir (Node-only tool imports) and is a heavier follow-up.
//
// Run: (from e2e/)  node management_official_worker_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38292);

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  const { server, baseUrl } = spawnServer('worker', PORT);
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    // A self-hosted environment + a session on it → the queue holds a seeded
    // healthcheck plus the session work item.
    const env = await client.beta.environments.create({
      name: 'official-worker-env',
      config: { type: 'self_hosted' },
      betas: BETAS,
    });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: env.id,
      betas: BETAS,
    });
    const queued = await drain(client.beta.environments.work.list(env.id, { betas: BETAS }));
    assert.deepEqual(
      queued.map((w) => w.data.type).sort(),
      ['healthcheck', 'session'],
      'the queue holds the seeded healthcheck + the session work',
    );
    pass('self-hosted env enqueues healthcheck + session work');

    // Drive the queue with the OFFICIAL WorkPoller (the reference worker's control
    // plane): non-blocking drain — claim each item, ack it, yield it, force-stop it,
    // and return when the queue empties. If awaken's poll/ack/stop wire matches what
    // the official worker expects, this iterates awaken's items unmodified.
    const seen = [];
    for await (const work of client.beta.environments.work.poller({
      environmentId: env.id,
      environmentKey: 'e2e-env-key',
      drain: true,
      blockMs: null,
    })) {
      assert.equal(work.state, 'active', 'the official poller claims each item into the active lease');
      seen.push(work.data.type);
      if (work.data.type === 'session') {
        assert.equal(work.data.id, session.id, 'the session work item references the session');
      }
    }
    assert.ok(
      seen.includes('healthcheck') && seen.includes('session'),
      `the official WorkPoller claimed awaken's queued items (saw: ${seen.join(',')})`,
    );
    pass('the official SDK WorkPoller drives awaken’s work queue unmodified (poll→ack→stop)');

    // After the reference worker drained it, the queue is empty (every item stopped).
    const after = await drain(client.beta.environments.work.list(env.id, { betas: BETAS }));
    assert.ok(
      after.every((w) => w.state !== 'queued' && w.state !== 'active'),
      `no item is left queued/active after the official worker drained (states: ${after.map((w) => w.state)})`,
    );
    pass('the queue is fully drained after the official worker runs');

    console.log('E2E PASS: awaken’s work queue is driven by the official SDK EnvironmentWorker helper.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server);
  }
}

main();
