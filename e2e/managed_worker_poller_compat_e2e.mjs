// The official SDK's self-hosted WORK POLLER helper drives our queue (Managed
// Agents compatibility, #1). management_self_hosted_worker_e2e drives the queue
// with the raw `work.*` client; this proves the higher-level, pre-built
// `WorkPoller` helper (`@anthropic-ai/sdk/helpers/beta/environments`, the auto-poll
// loop `EnvironmentWorker` builds on) claims our `session` work item unchanged —
// i.e. Anthropic's own worker code, not just its client, is compatible with our
// control plane. Offline: no real API, the official helper is pointed at localhost.
//
// Run: (from e2e/)  node managed_worker_poller_compat_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { WorkPoller } from '@anthropic-ai/sdk/helpers/beta/environments';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38291);

async function drain(page) {
  const items = [];
  for await (const item of page) items.push(item);
  return items;
}

async function main() {
  const { server, baseUrl } = spawnServer('worker', PORT);
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const env = await client.beta.environments.create({
      name: 'self-hosted-poller',
      config: { type: 'self_hosted' },
      betas: BETAS,
    });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: env.id,
      betas: BETAS,
    });

    // The official pre-built poller (the loop EnvironmentWorker.run uses): a single
    // non-blocking drain pass. `autoStop: false` — we free each lease ourselves so
    // the single-active-lease cap lets the next poll reach the session work.
    const poller = new WorkPoller({
      client,
      environmentId: env.id,
      environmentKey: 'e2e-dummy',
      blockMs: null,
      drain: true,
      autoStop: false,
      requestOptions: { betas: BETAS },
    });

    const claimed = [];
    let sessionWork = null;
    for await (const work of poller) {
      claimed.push(work.data.type);
      assert.equal(work.state, 'active', 'the official poller leased an active item');
      if (work.data.type === 'session') {
        sessionWork = work;
        await client.beta.environments.work.stop(work.id, { environment_id: env.id, betas: BETAS });
        break;
      }
      // Free the healthcheck lease so the next poll reaches the session work.
      await client.beta.environments.work.stop(work.id, { environment_id: env.id, betas: BETAS });
    }

    assert.ok(sessionWork, `the official poller claimed the session work, saw: ${claimed}`);
    assert.equal(sessionWork.data.id, session.id, 'poller work.data.id maps to our session id');
    pass('official WorkPoller helper claims our session work item unchanged');

    // The queue is drained of unfinished work (both items stopped).
    const remaining = await drain(client.beta.environments.work.list(env.id, { betas: BETAS }));
    assert.ok(
      remaining.every((w) => w.state === 'stopped'),
      `all claimed work is stopped, states: ${remaining.map((w) => w.state)}`,
    );
    pass('the official poller drained the queue (all work stopped)');

    console.log('E2E PASS: official SDK WorkPoller helper is compatible with our self-hosted queue.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server);
  }
}

main();
