// Every public TypeScript Environment Work method with state/CAS negatives.
//
// Cause/effect graph: queue state + lease owner + heartbeat precondition +
// request mutation -> DTO/status + one durable state transition.
// Decision table: queued work is retrievable/updatable/claimable; one owner may
// ack/heartbeat/stop; stale CAS, unknown ids, invalid poll bounds never mutate.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { pass, spawnServer, stopServer, waitForPort } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38342);

async function drain(page) {
  const rows = [];
  for await (const row of page) rows.push(row);
  return rows;
}

function status(expected) {
  return (error) => error?.status === expected;
}

const { server, baseUrl: baseURL } = spawnServer('worker', PORT);
try {
  await waitForPort(PORT);
  const admin = new Anthropic({ apiKey: 'e2e-dummy', baseURL });
  const worker = new Anthropic({ authToken: 'e2e-env-key', baseURL }); // awaken-allow: secret
  const environment = await admin.beta.environments.create({
    name: 'environment-work-depth',
    config: { type: 'self_hosted' },
    betas: BETAS,
  });

  const initial = await drain(admin.beta.environments.work.list(environment.id, {
    limit: 1,
    betas: BETAS,
  }));
  assert.equal(initial.length, 1);
  const work = initial[0];

  const retrieved = await admin.beta.environments.work.retrieve(work.id, {
    environment_id: environment.id,
    betas: BETAS,
  });
  assert.equal(retrieved.id, work.id);
  assert.equal(retrieved.state, 'queued');

  const patched = await admin.beta.environments.work.update(work.id, {
    environment_id: environment.id,
    metadata: { owner: 'one', remove_me: 'yes' },
    betas: BETAS,
  });
  assert.deepEqual(patched.metadata, { owner: 'one', remove_me: 'yes' });
  const merged = await admin.beta.environments.work.update(work.id, {
    environment_id: environment.id,
    metadata: { owner: 'two', remove_me: null },
    betas: BETAS,
  });
  assert.deepEqual(merged.metadata, { owner: 'two' });
  pass('Environment Work retrieve/update preserve metadata patch semantics');

  const claimed = await worker.beta.environments.work.poll(environment.id, {
    block_ms: null,
    'Anthropic-Worker-ID': 'worker-depth',
    betas: BETAS,
  });
  assert.equal(claimed.id, work.id);
  assert.equal(claimed.state, 'active');
  const acked = await worker.beta.environments.work.ack(work.id, {
    environment_id: environment.id,
    betas: BETAS,
  });
  assert.ok(acked.acknowledged_at);

  const firstBeat = await worker.beta.environments.work.heartbeat(work.id, {
    environment_id: environment.id,
    expected_last_heartbeat: 'NO_HEARTBEAT',
    desired_ttl_seconds: 2,
    betas: BETAS,
  });
  assert.equal(firstBeat.lease_extended, true);
  assert.ok(firstBeat.last_heartbeat);
  const secondBeat = await worker.beta.environments.work.heartbeat(work.id, {
    environment_id: environment.id,
    expected_last_heartbeat: firstBeat.last_heartbeat,
    desired_ttl_seconds: 2,
    betas: BETAS,
  });
  assert.equal(secondBeat.lease_extended, true);
  await assert.rejects(
    () => worker.beta.environments.work.heartbeat(work.id, {
      environment_id: environment.id,
      expected_last_heartbeat: firstBeat.last_heartbeat,
      desired_ttl_seconds: 2,
      betas: BETAS,
    }),
    status(412),
  );
  assert.equal(
    (await admin.beta.environments.work.retrieve(work.id, {
      environment_id: environment.id,
      betas: BETAS,
    })).latest_heartbeat_at,
    secondBeat.last_heartbeat,
    'stale heartbeat is atomic',
  );
  pass('Environment Work heartbeat is an owner-scoped CAS chain');

  const stats = await admin.beta.environments.work.stats(environment.id, { betas: BETAS });
  assert.ok(Object.values(stats).some((value) => value === 1), JSON.stringify(stats));

  for (const block_ms of [0, 1000]) {
    await assert.rejects(
      () => worker.beta.environments.work.poll(environment.id, { block_ms, betas: BETAS }),
      status(400),
    );
  }
  pass('Environment Work poll/stats use official SDK DTOs and reject boundary violations');

  const stopped = await worker.beta.environments.work.stop(work.id, {
    environment_id: environment.id,
    force: true,
    betas: BETAS,
  });
  assert.equal(stopped.state, 'stopped');
  await assert.rejects(
    () => worker.beta.environments.work.stop(work.id, {
      environment_id: environment.id,
      force: true,
      betas: BETAS,
    }),
    status(409),
    'the official EnvironmentWorker recognizes 409 as already stopped',
  );

  // Stop-mode causal graph: WorkPoller calls stop after its handler returns
  // with `force` omitted; direct callers may spell false; EnvironmentWorker
  // force-stops during exceptional cleanup. In this self-hosted protocol the
  // caller owns process cleanup, while the queue owns only the durable lease,
  // so all three causes must converge to one stopped item and release capacity.
  // Decision table: W0 null -> 400/no mutation, W1 omitted -> stopped, W2
  // false -> stopped, W3 true above -> stopped, W4 repeat any terminal request
  // -> 409. Leaving W1/W2 in `stopping` would retain the single-active lease
  // and deadlock the official WorkPoller on its next poll.
  for (const [label, force] of [['omitted', undefined], ['false', false]]) {
    const stopEnvironment = await admin.beta.environments.create({
      name: `environment-work-stop-${label}`,
      config: { type: 'self_hosted' },
      betas: BETAS,
    });
    const stopWork = await worker.beta.environments.work.poll(stopEnvironment.id, {
      block_ms: null,
      'Anthropic-Worker-ID': `worker-stop-${label}`,
      betas: BETAS,
    });
    assert.ok(stopWork, `W1/W2 ${label} poll claims the healthcheck`);
    if (force === undefined) {
      const invalidNull = await fetch(
        `${baseURL}/v1/environments/${stopEnvironment.id}/work/${stopWork.id}/stop`,
        {
          method: 'POST',
          headers: {
            authorization: 'Bearer e2e-env-key',
            'anthropic-beta': BETAS[0],
            'content-type': 'application/json',
          },
          body: JSON.stringify({ force: null }),
        },
      );
      assert.equal(invalidNull.status, 400, 'W0 non-nullable force fails closed');
      assert.equal(
        (await admin.beta.environments.work.retrieve(stopWork.id, {
          environment_id: stopEnvironment.id,
          betas: BETAS,
        })).state,
        'active',
        'W0 invalid stop has no lease mutation',
      );
    }
    const stopParams = {
      environment_id: stopEnvironment.id,
      ...(force === undefined ? {} : { force }),
      betas: BETAS,
    };
    const terminal = await worker.beta.environments.work.stop(stopWork.id, stopParams);
    assert.equal(terminal.state, 'stopped', `W1/W2 ${label}`);
    assert.ok(terminal.stop_requested_at, `W1/W2 ${label} records request time`);
    assert.ok(terminal.stopped_at, `W1/W2 ${label} records terminal time`);
    assert.equal(
      (await admin.beta.environments.work.stats(stopEnvironment.id, { betas: BETAS })).pending,
      0,
      `W1/W2 ${label} releases the active lease`,
    );
  }
  pass('Environment Work stop omitted/false/true modes converge after caller-owned cleanup');

  const beforeUnknown = await drain(admin.beta.environments.work.list(environment.id, { betas: BETAS }));
  const unknown = 'work_does_not_exist';
  const unknownCalls = [
    () => admin.beta.environments.work.retrieve(unknown, {
      environment_id: environment.id, betas: BETAS,
    }),
    () => admin.beta.environments.work.update(unknown, {
      environment_id: environment.id, metadata: { x: 'y' }, betas: BETAS,
    }),
    () => worker.beta.environments.work.ack(unknown, {
      environment_id: environment.id, betas: BETAS,
    }),
    () => worker.beta.environments.work.heartbeat(unknown, {
      environment_id: environment.id, expected_last_heartbeat: 'NO_HEARTBEAT', betas: BETAS,
    }),
    () => worker.beta.environments.work.stop(unknown, {
      environment_id: environment.id, force: true, betas: BETAS,
    }),
  ];
  for (const call of unknownCalls) await assert.rejects(call, status(404));
  const afterUnknown = await drain(admin.beta.environments.work.list(environment.id, { betas: BETAS }));
  assert.deepEqual(
    afterUnknown.map((row) => [row.id, row.state, row.metadata]),
    beforeUnknown.map((row) => [row.id, row.state, row.metadata]),
    'unknown method calls have no queue side effects',
  );
  pass('all Environment Work item methods fail closed on unknown ids');
} finally {
  await stopServer(server);
}

console.log('E2E PASS: every public TypeScript Environment Work method and negative boundary.');
