import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';

import {
  allowManagedToolBoundaries,
  availablePort,
  deploymentEnv,
  ensureProductionBuilt,
  initializeE2EInstallation,
  scenarioMemoryStore,
  stopServer,
  trackSpawnedServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

async function main() {
  // Deterministic Memory fixture decision table: C1 one well-formed Store on a
  // terminal PageCursor -> E1 return its server-owned id; C2 empty, multiple,
  // paginated, or malformed data -> E2 reject without selecting the first row.
  // Rules M1=C1=>E1 and M2=C2=>E2 keep Rust publication as the only seed-id owner.
  const memoryHeaders = { 'anthropic-beta': 'agent-memory-2026-07-22' };
  const memoryStore = { id: 'server-owned-memory' };
  const memoryListClient = (page) => ({
    get: async (route, { headers }) => {
      assert.equal(route, '/v1/memory_stores', 'M1/M2 use the existing list route');
      assert.equal(headers, memoryHeaders, 'M1/M2 preserve caller beta headers');
      return page;
    },
  });
  assert.equal(
    await scenarioMemoryStore(
      memoryListClient({ data: [memoryStore], next_page: null }),
      memoryHeaders,
    ),
    memoryStore,
    'M1 returns the sole server-owned fixture identity',
  );
  for (const [rule, page] of [
    ['M2 empty', { data: [], next_page: null }],
    ['M2 multiple', { data: [memoryStore, { id: 'another' }], next_page: null }],
    ['M2 paginated', { data: [memoryStore], next_page: 'cursor' }],
    ['M2 malformed page', { data: null, next_page: null }],
    ['M2 malformed store', { data: [{}], next_page: null }],
  ]) {
    await assert.rejects(
      scenarioMemoryStore(memoryListClient(page), memoryHeaders),
      /must publish exactly one MemoryStore/u,
      rule,
    );
  }

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

  // Explicit-installation cause/effect table:
  // C1=unseen exact config; C2=same successful config; C3=the CLI rejects a
  // config; C4=durable bytes become corrupt after success; C5=config bytes,
  // path, cwd, or resolved binary identity changes. Effects: E1=C1 runs
  // production initialization once; E2=C2 skips it;
  // E3=C3 throws on every attempt and never caches; E4=C4 remains untouched so
  // ordinary Serve can fail closed; E5=C5 is a new observation. Constraints:
  // the cache identity is cwd+absolute config path+SHA-256(config bytes)+the
  // resolved binary path+SHA-256(binary bytes), and no branch owns migration or
  // legacy-adoption policy. Rules I1-I5 correspond to C1-C5.
  const installationRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-harness-install-'));
  try {
    const dataDir = path.join(installationRoot, 'durable');
    const environment = {
      ...process.env,
      ...deploymentEnv(dataDir, { identityMode: 'no-login' }),
    };
    const configPath = path.join(environment.HOME, '.awaken', 'config.toml');
    const sessionsPath = path.join(dataDir, 'sessions.db');
    const markerPath = path.join(dataDir, 'platform-workspace-id');

    assert.equal(initializeE2EInstallation(environment), 'initialized', 'I1');
    assert.ok(fs.statSync(sessionsPath).size > 0, 'I1 canonical Session storage exists');
    assert.ok(fs.statSync(markerPath).size > 0, 'I1 workspace identity exists');
    assert.equal(initializeE2EInstallation(environment), 'cached', 'I2');

    fs.appendFileSync(configPath, '# exact config bytes changed\n');
    assert.equal(initializeE2EInstallation(environment), 'initialized', 'I5 bytes');
    assert.equal(initializeE2EInstallation(environment), 'cached', 'I2 after I5 bytes');
    const alternateConfigPath = path.join(installationRoot, 'alternate-config.toml');
    fs.copyFileSync(configPath, alternateConfigPath);
    assert.equal(
      initializeE2EInstallation(environment, { configPath: alternateConfigPath }),
      'initialized',
      'I5 path',
    );
    const alternateCwd = path.join(installationRoot, 'alternate-cwd');
    fs.mkdirSync(alternateCwd);
    assert.equal(
      initializeE2EInstallation(environment, { cwd: alternateCwd }),
      'initialized',
      'I5 cwd',
    );
    const alternateBinary = path.join(installationRoot, 'alternate-awaken');
    fs.copyFileSync(ensureProductionBuilt(), alternateBinary);
    fs.chmodSync(alternateBinary, 0o755);
    assert.equal(
      initializeE2EInstallation(environment, { binary: alternateBinary }),
      'initialized',
      'I5 resolved binary identity',
    );
    assert.equal(
      initializeE2EInstallation(environment, { binary: alternateBinary }),
      'cached',
      'I2 after I5 resolved binary identity',
    );

    fs.truncateSync(sessionsPath, 0);
    assert.equal(initializeE2EInstallation(environment), 'cached', 'I4');
    assert.equal(fs.statSync(sessionsPath).size, 0, 'I4 cache never repairs durable damage');

    const invalidDir = path.join(installationRoot, 'invalid');
    const invalidEnvironment = {
      ...process.env,
      ...deploymentEnv(invalidDir, { identityMode: 'no-login' }),
    };
    fs.writeFileSync(
      path.join(invalidEnvironment.HOME, '.awaken', 'config.toml'),
      'data_dir = [\n',
    );
    for (const rule of ['I3 first attempt', 'I3 retry']) {
      assert.throws(
        () => initializeE2EInstallation(invalidEnvironment),
        /explicit E2E installation failed/u,
        rule,
      );
    }
  } finally {
    fs.rmSync(installationRoot, { recursive: true, force: true });
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
    [{
      id: 'old-idle',
      type: 'session.status_idle',
      processed_at: 't0',
      stop_reason: { type: 'end_turn' },
    }],
    [
      {
        id: 'old-idle',
        type: 'session.status_idle',
        processed_at: 't0',
        stop_reason: { type: 'end_turn' },
      },
      { id: 'receipt', type: 'user.message', processed_at: null },
    ],
    [
      {
        id: 'old-idle',
        type: 'session.status_idle',
        processed_at: 't0',
        stop_reason: { type: 'end_turn' },
      },
      { id: 'receipt', type: 'user.message', processed_at: 't1' },
    ],
    [
      {
        id: 'old-idle',
        type: 'session.status_idle',
        processed_at: 't0',
        stop_reason: { type: 'end_turn' },
      },
      { id: 'receipt', type: 'user.message', processed_at: 't1' },
      {
        id: 'new-idle',
        type: 'session.status_idle',
        processed_at: 't2',
        stop_reason: { type: 'end_turn' },
      },
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

  // Approval-driver terminal-scope table: C1 the exact task receipt is
  // processed; C2 history contains an end_turn before that receipt; C3 a new
  // end_turn is committed after it. Effects: E1=C1+C2&&!C3 keeps waiting and
  // never borrows the old turn; E2=C1+C3 returns terminal history. Rules B1 and
  // B2 prove the canonical approval driver remains receipt-scoped even when no
  // approval boundary is needed; gated live scenarios own the send branch.
  reads = 2;
  listRequests.length = 0;
  const terminalEvents = await allowManagedToolBoundaries({
    client: fakeClient,
    sessionId: 'session',
    taskReceiptId: 'receipt',
    betas: ['managed-agents-test'],
    description: 'B1-B2 receipt-scoped terminal',
    timeoutMs: 1_000,
    maxBoundaries: 1,
  });
  assert.equal(reads, 4, 'B1 ignores the old end_turn and B2 observes the new one');
  assert.deepEqual(
    terminalEvents.map((event) => event.id),
    ['old-idle', 'receipt', 'new-idle'],
    'B2 returns the complete history only after the receipt-scoped terminal',
  );

  await Promise.all([stopServer(trackedExit), stopServer(explicitExit)]);
  console.log('E2E PASS: harness child lifecycle readiness decision table.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
