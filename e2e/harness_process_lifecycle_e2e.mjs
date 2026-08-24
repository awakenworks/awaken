import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';

import {
  availablePort,
  deploymentEnv,
  stopServer,
  trackSpawnedServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

async function main() {
  // Fixture-identity cause/effect table: C1=identity omitted/empty;
  // C2=an explicit no-login, self-managed, or Awaken Cloud selection.
  // E1=C1 fails before creating deployment state; E2=C2 writes that exact
  // value once. Rules I1=C1=>E1 and I2=C2=>E2 keep hermetic fixtures from
  // silently inheriting the product's intentional local Cloud default while
  // leaving the product parser as the identity-vocabulary authority.
  const identityRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-harness-identity-'));
  try {
    assert.throws(
      () => deploymentEnv(path.join(identityRoot, 'missing')),
      /requires an explicit identityMode/u,
      'I1',
    );
    for (const identityMode of ['no-login', 'self-managed', 'awaken-cloud']) {
      const env = deploymentEnv(path.join(identityRoot, identityMode), { identityMode });
      const config = fs.readFileSync(path.join(env.HOME, '.awaken', 'config.toml'), 'utf8');
      assert.equal(
        config.match(/^identity_mode = .*$/gmu)?.join('\n'),
        `identity_mode = ${JSON.stringify(identityMode)}`,
        `I2 ${identityMode}`,
      );
    }
  } finally {
    fs.rmSync(identityRoot, { recursive: true, force: true });
  }

  // Cause/effect graph: port ownership (tracked child / explicit child / no
  // child) × child state (listening / terminal) -> readiness or bounded error.
  // Decision table: R1 tracked+terminal -> immediate child-exit error; R2
  // explicit+terminal -> same error; R3 external/unowned -> deadline error;
  // R4 tracked+listening -> readiness success. These rules prevent a crashed
  // E2E server from being reported only after the 15-minute port deadline.
  const trackedPort = await availablePort(21_000);
  const trackedExit = trackSpawnedServer(
    trackedPort,
    spawn(process.execPath, ['-e', 'process.exit(23)'], { stdio: 'ignore' }),
  );
  await new Promise((resolve) => trackedExit.once('exit', resolve));
  await assert.rejects(
    waitForPort(trackedPort, 5_000),
    /server exited before it listened.*code=23/u,
    'R1',
  );

  const explicitPort = await availablePort(trackedPort + 1);
  const explicitExit = spawn(process.execPath, ['-e', 'process.exit(24)'], { stdio: 'ignore' });
  await assert.rejects(
    waitForPort(explicitPort, 5_000, explicitExit),
    /server exited before it listened.*code=24/u,
    'R2',
  );

  const spawnFailurePort = await availablePort(explicitPort + 1);
  const spawnFailure = spawn('/definitely-not-an-awaken-e2e-binary', [], { stdio: 'ignore' });
  await assert.rejects(
    waitForPort(spawnFailurePort, 5_000, spawnFailure),
    /server exited before it listened.*spawn=ENOENT/u,
    'R2b',
  );
  await stopServer(spawnFailure);

  const externalPort = await availablePort(spawnFailurePort + 1);
  await assert.rejects(
    waitForPort(externalPort, 50),
    /server did not listen/u,
    'R3',
  );

  const listeningPort = await availablePort(externalPort + 1);
  const listening = trackSpawnedServer(
    listeningPort,
    spawn(
      process.execPath,
      [
        '-e',
        `require('node:net').createServer().listen(${listeningPort}, '127.0.0.1')`,
      ],
      { stdio: 'ignore' },
    ),
  );
  try {
    await waitForPort(listeningPort, 5_000);
  } finally {
    await stopServer(listening);
  }

  // Receipt-observation cause/effect table: C1=exact receipt absent, C2=present
  // but unprocessed, C3=processed with an older terminal only, C4=later scenario
  // effect committed; C5=the caller omits beta and supplies SDK list params.
  // Effects: E1=C1|C2|C3 retries; E2=C4 returns full history, exact receipt,
  // and the post-receipt delta; E3=C5 forwards list params while leaving beta
  // absent. Constraint: the adapter must use the caller predicate, cannot
  // prescribe a terminal, and cannot manufacture a beta default. Decision
  // rules: W1 C1=>E1; W2 C2=>E1; W3 C3=>E1; W4 C4=>E2; W5 invalid id=>fail
  // before IO; W6 C4+C5=>E2+E3.
  const snapshots = [
    [{ id: 'old-idle', type: 'session.status_idle', processed_at: 't0' }],
    [
      { id: 'old-idle', type: 'session.status_idle', processed_at: 't0' },
      { id: 'receipt', type: 'user.message', processed_at: null },
    ],
    [
      { id: 'old-idle', type: 'session.status_idle', processed_at: 't0' },
      { id: 'receipt', type: 'user.message', processed_at: 't1' },
    ],
    [
      { id: 'old-idle', type: 'session.status_idle', processed_at: 't0' },
      { id: 'receipt', type: 'user.message', processed_at: 't1' },
      { id: 'new-idle', type: 'session.status_idle', processed_at: 't2' },
    ],
  ];
  let reads = 0;
  const listRequests = [];
  const fakeClient = {
    beta: {
      sessions: {
        events: {
          list: async function* list(sessionId, params) {
            assert.equal(sessionId, 'session');
            listRequests.push(params);
            const snapshot = snapshots[Math.min(reads, snapshots.length - 1)];
            reads += 1;
            yield* snapshot;
          },
        },
      },
    },
  };
  const observed = await waitForSessionEventReceipt(
    fakeClient,
    'session',
    'receipt',
    ['managed-agents-test'],
    ({ delta }) => delta.some((event) => event.id === 'new-idle'),
    'W1-W4 fake receipt lifecycle',
    { timeoutMs: 1_000, pollMs: 1 },
  );
  assert.equal(reads, 4, 'W1-W4 every incomplete observation retried exactly once');
  assert.equal(observed.receiptEvent.id, 'receipt');
  assert.deepEqual(observed.delta.map((event) => event.id), ['new-idle']);
  assert.deepEqual(observed.events.map((event) => event.id), ['old-idle', 'receipt', 'new-idle']);
  assert.ok(listRequests.every((params) => params.betas?.[0] === 'managed-agents-test'));

  reads = snapshots.length - 1;
  listRequests.length = 0;
  await waitForSessionEventReceipt(
    fakeClient,
    'session',
    'receipt',
    undefined,
    ({ delta }) => delta.some((event) => event.id === 'new-idle'),
    'W6 SDK-owned beta with caller pagination',
    { timeoutMs: 1_000, pollMs: 1, listParams: { limit: 1 } },
  );
  assert.deepEqual(listRequests, [{ limit: 1 }], 'W6 beta stays absent and limit is forwarded');
  await assert.rejects(
    waitForSessionEventReceipt(fakeClient, 'session', '', [], () => true, 'W5'),
    /exact Managed Event receipt id/u,
  );

  await Promise.all([stopServer(trackedExit), stopServer(explicitExit)]);
  console.log('E2E PASS: harness child lifecycle readiness decision table.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
