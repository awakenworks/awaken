// EnvironmentWorker differential: drive awaken's self-hosted work queue with the
// OFFICIAL SDK worker helper, not hand-rolled poll/ack/stop calls. The reference
// baseline the matrix said was "missing" ships inside @anthropic-ai/sdk as
// `client.beta.environments.work.poller` (WorkPoller) / `.worker` (EnvironmentWorker).
// If awaken's work-queue API is compatible with the official worker, the official
// poller claims, acks, yields, and stops awaken's queued items unmodified —
// which is exactly the "vs the official EnvironmentWorker" conformance check.
//
// This uses the control-plane WorkPoller (poll -> ack -> yield -> stop) as the
// reference; the full EnvironmentWorker.run() additionally downloads skills + runs
// the agent toolset in a workdir (Node-only tool imports) and is a heavier follow-up.
//
// Run: (from e2e/)  node management_official_worker_e2e.mjs

import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, waitForValue, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38292);

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  const { server, baseUrl } = spawnServer('worker', PORT);
  const workdir = mkdtempSync(join(tmpdir(), 'awaken-official-worker-'));
  try {
    await waitForPort(PORT);
    const stopCalls = new Map();
    const heartbeatStates = new Map();
    const client = new Anthropic({
      apiKey: 'e2e-dummy',
      baseURL: baseUrl,
      fetch: async (input, init) => {
        const request = new Request(input, init);
        const match = new URL(request.url).pathname.match(/\/work\/([^/]+)\/stop$/);
        if (request.method === 'POST' && match) {
          const body = await request.clone().text();
          const id = decodeURIComponent(match[1]);
          stopCalls.set(id, [...(stopCalls.get(id) ?? []), body ? JSON.parse(body).force : undefined]);
        }
        // Observe the request without replacing the SDK's transport arguments.
        // In particular its original AbortSignal must reach native fetch.
        const response = await fetch(input, init);
        const heartbeat = new URL(request.url).pathname.match(/\/work\/([^/]+)\/heartbeat$/);
        if (request.method === 'POST' && heartbeat && response.ok) {
          const id = decodeURIComponent(heartbeat[1]);
          const { state } = await response.clone().json();
          heartbeatStates.set(id, [...(heartbeatStates.get(id) ?? []), state]);
        }
        return response;
      },
    });

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
    // plane): non-blocking drain — claim each item, ack it, yield it, stop it,
    // and return when the queue empties. If awaken's poll/ack/stop wire matches what
    // the official worker expects, this iterates awaken's items unmodified.
    const seen = [];
    const seenIds = [];
    for await (const work of client.beta.environments.work.poller({
      environmentId: env.id,
      environmentKey: 'e2e-env-key',
      drain: true,
      blockMs: null,
    })) {
      assert.equal(work.state, 'active', 'the official poller claims each item into the active lease');
      seen.push(work.data.type);
      seenIds.push(work.id);
      if (work.data.type === 'session') {
        assert.equal(work.data.id, session.id, 'the session work item references the session');
      }
    }
    assert.ok(
      seen.includes('healthcheck') && seen.includes('session'),
      `the official WorkPoller claimed awaken's queued items (saw: ${seen.join(',')})`,
    );
    for (const id of seenIds) {
      assert.deepEqual(stopCalls.get(id), [undefined], 'WorkPoller releases completed work with ordinary stop');
    }
    pass('the official SDK WorkPoller drives awaken’s work queue unmodified (poll→ack→stop)');

    // After the reference worker drained it, the queue is empty (every item stopped).
    const after = await drain(client.beta.environments.work.list(env.id, { betas: BETAS }));
    assert.ok(
      after.every((w) => w.state !== 'queued' && w.state !== 'active'),
      `no item is left queued/active after the official worker drained (states: ${after.map((w) => w.state)})`,
    );
    pass('the queue is fully drained after the official worker runs');

    // Portable EnvironmentWorker decision rule: C1 a fresh self-hosted
    // Environment has one healthcheck; C2 the official WorkPoller drains it;
    // C3 creating a Session enqueues one Session item; C4 the unmodified
    // EnvironmentWorker owns poll/ack/handle/heartbeat; C5 its process owner
    // cancels after observing lease health. C1+C2+C3+C4+C5 -> the Session item
    // reaches stopped while retaining acknowledgement and heartbeat evidence.
    // This helper contract needs no mount namespace. Skill/Memory filesystem
    // realization remains in management_environment_worker_full_e2e.mjs, whose
    // bwrap requirement is a platform capability gate rather than the sole SDK
    // method-compatibility proof.
    const workerEnvironment = await client.beta.environments.create({
      name: 'official-environment-worker',
      config: { type: 'self_hosted' },
      betas: BETAS,
    });
    for await (const _ of client.beta.environments.work.poller({
      environmentId: workerEnvironment.id,
      environmentKey: 'e2e-env-key',
      drain: true,
      blockMs: null,
    })) {
      // The poller owns acknowledgement and ordinary stop for the healthcheck.
    }
    const workerSession = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: workerEnvironment.id,
      betas: BETAS,
    });
    const controller = new AbortController();
    const worker = client.beta.environments.work.worker({
      environmentId: workerEnvironment.id,
      environmentKey: 'e2e-env-key',
      workdir,
      tools: [],
      maxIdleMs: 50,
      signal: controller.signal,
    });
    const running = worker.run();
    const active = await waitForValue(
      async () => (await drain(client.beta.environments.work.list(
        workerEnvironment.id,
        { betas: BETAS },
      ))).find((work) => work.data.type === 'session' && work.data.id === workerSession.id),
      (work) => work?.state === 'active'
        && work.acknowledged_at !== null
        && work.latest_heartbeat_at !== null,
      'the official EnvironmentWorker did not acknowledge and heartbeat its Session item',
      { timeoutMs: 20_000, pollMs: 40 },
    );
    controller.abort();
    await running;
    assert.ok(active.acknowledged_at, 'EnvironmentWorker acknowledged its claimed item');
    assert.ok(active.latest_heartbeat_at, 'EnvironmentWorker maintained its claimed lease');
    const stopped = await waitForValue(
      async () => client.beta.environments.work.retrieve(active.id, {
        environment_id: workerEnvironment.id,
        betas: BETAS,
      }),
      (work) => work.state === 'stopped',
      'the official EnvironmentWorker did not force-stop after cancellation',
    );
    assert.ok(stopped.stopped_at, 'EnvironmentWorker cancellation settles lease ownership');
    assert.deepEqual(stopCalls.get(active.id), [true], 'EnvironmentWorker requests forced stop on cancellation');
    pass('the official SDK EnvironmentWorker settles Awaken Session work unmodified');

    // Graceful control-plane stop is not a lease release: heartbeat must tell
    // the unmodified Worker to exit, and its own forced stop settles the lease.
    const gracefulSession = await client.beta.sessions.create({
      agent: 'assistant', environment_id: workerEnvironment.id, betas: BETAS,
    });
    const gracefulController = new AbortController();
    const gracefulRunning = client.beta.environments.work.worker({
      environmentId: workerEnvironment.id, environmentKey: 'e2e-env-key',
      workdir, tools: [], maxIdleMs: 50, signal: gracefulController.signal,
    }).run();
    try {
      const granted = await waitForValue(
        async () => (await drain(client.beta.environments.work.list(workerEnvironment.id, { betas: BETAS })))
          .find((work) => work.data.type === 'session' && work.data.id === gracefulSession.id),
        (work) => work?.state === 'active' && work.latest_heartbeat_at !== null,
        'the official Worker did not heartbeat the graceful-stop Session',
        { timeoutMs: 20_000, pollMs: 40 },
      );
      const stopping = await client.beta.environments.work.stop(granted.id, {
        environment_id: workerEnvironment.id, force: false, betas: BETAS,
      }, { headers: { Authorization: 'Bearer e2e-env-key' } });
      assert.equal(stopping.state, 'stopping');
      assert.equal(stopping.stopped_at, null);
      const settled = await waitForValue(
        () => client.beta.environments.work.retrieve(granted.id, { environment_id: workerEnvironment.id, betas: BETAS }),
        (work) => work.state === 'stopped',
        'heartbeat did not lead the official Worker to confirm graceful shutdown',
        // The locked SDK renews this 60-second lease every 30 seconds.
        // Allow one real heartbeat plus shutdown; do not replace its timer.
        { timeoutMs: 45_000, pollMs: 40 },
      );
      assert.equal(settled.stop_requested_at, stopping.stop_requested_at);
      assert.ok(heartbeatStates.get(granted.id)?.includes('stopping'), 'the real SDK heartbeat observed the stop request');
      assert.deepEqual(stopCalls.get(granted.id), [false, true]);
      pass('graceful stop reaches the official Worker through heartbeat before forced completion');
    } finally {
      gracefulController.abort();
      let shutdownDeadline;
      try {
        await Promise.race([
          gracefulRunning,
          new Promise((_, reject) => {
            shutdownDeadline = setTimeout(() => reject(new Error('the official Worker did not exit after abort')), 10_000);
          }),
        ]);
      } finally {
        clearTimeout(shutdownDeadline);
      }
    }

    console.log('E2E PASS: awaken’s work queue is driven by the official SDK EnvironmentWorker helper.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server);
    rmSync(workdir, { recursive: true, force: true });
  }
}

main();
