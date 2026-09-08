import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import net from 'node:net';
import { once } from 'node:events';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';

import {
  allowManagedToolBoundaries,
  availablePort,
  availablePortBlock,
  deploymentEnv,
  ensureProductionBuilt,
  initializeE2EInstallation,
  scenarioMemoryStore,
  spawnServer,
  stopServer,
  trackSpawnedServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

async function main() {
  // Port-selection cause/effect table: C1 no caller assignment; C2 explicit
  // server/Worker assignment; C3 the preferred socket is occupied; C4 invalid
  // port input; C5 the previous listener has fully closed. Effects: P1 C1 ->
  // defaults outside Linux's 32768..60999 ephemeral range; P2 C2 -> preserve
  // exact assignments; P3 C3 -> another port without closing the foreign owner;
  // P4 C4 -> reject instead of hiding configuration errors; P5 C5 -> reuse the
  // preferred port. Bind/probe/close is not an atomic cross-process reservation;
  // the existing child-exit readiness rules below must still reject a lost race.
  // These are harness contracts. Real restart scenarios retain their own
  // durable-state assertions and consume this same E2E_PORT authority.
  for (const [rule, assignments] of [
    ['P1', {}],
    ['P1 occupied offset', {}],
    ['P2', { E2E_PORT: '23117', E2E_WORKER_PORT: '23167' }],
  ]) {
    const env = { ...process.env };
    delete env.E2E_PORT;
    delete env.E2E_WORKER_PORT;
    Object.assign(env, assignments);
    const child = spawnSync(process.execPath, ['--input-type=module', '-e', `
      import net from 'node:net';
      const preferred = 20_000 + (process.pid % 100) * 100;
      const occupied = ${JSON.stringify(rule)} === 'P1 occupied offset' ? net.createServer() : null;
      if (occupied) await new Promise((resolve, reject) => {
        occupied.once('error', reject);
        occupied.listen(preferred + 99, '127.0.0.1', resolve);
      });
      await import(${JSON.stringify(new URL('./harness.mjs', import.meta.url).href)});
      if (occupied) {
        if (Number(process.env.E2E_PORT) === preferred) throw new Error('default block ignored occupied offset');
        await new Promise(resolve => occupied.close(resolve));
      }
      console.log(JSON.stringify([process.env.E2E_PORT, process.env.E2E_WORKER_PORT]));
    `], { env, encoding: 'utf8' });
    assert.equal(child.status, 0, `${rule}: ${child.stderr}`);
    const selected = JSON.parse(child.stdout.trim());
    if (rule === 'P2') assert.deepEqual(selected, ['23117', '23167'], rule);
    else assert.ok(selected.every(value => Number(value) >= 20_000 && Number(value) < 30_000), rule);
  }
  // Block-allocation decision table. B1 an entirely free block => exact reuse;
  // B2 an occupied base, Worker offset, or final offset => select another whole
  // block, preserve the foreign listener; B3 the listener closes => exact reuse;
  // B4 unaligned/out-of-range/non-integer input => reject. Boundary values 0,
  // 50 and 99 cover primary, Worker and largest relative-offset consumers.
  const block = await availablePortBlock(20_000);
  assert.equal(await availablePortBlock(block), block, 'B1');
  for (const offset of [0, 50, 99]) {
    const owner = net.createServer();
    await new Promise((resolve, reject) => {
      owner.once('error', reject);
      owner.listen(block + offset, '127.0.0.1', resolve);
    });
    try {
      assert.notEqual(await availablePortBlock(block), block, `B2 offset ${offset}`);
      assert.equal(owner.listening, true, 'B2 preserves foreign listener');
    } finally {
      await new Promise(resolve => owner.close(resolve));
    }
    assert.equal(await availablePortBlock(block), block, 'B3');
  }
  for (const invalid of [19_900, 30_000, 20_001, NaN]) {
    await assert.rejects(availablePortBlock(invalid), RangeError, 'B4');
  }

  // B5 all blocks have at least one occupied socket => bounded exhaustion,
  // never an unprobed fallback. Real listeners remain owned by this test; any
  // preexisting EADDRINUSE already supplies that block's obstruction.
  const blockers = [];
  try {
    for (let base = 20_000; base < 30_000; base += 100) {
      const owner = net.createServer();
      try {
        await new Promise((resolve, reject) => {
          owner.once('error', reject);
          owner.listen(base, '127.0.0.1', resolve);
        });
        blockers.push(owner);
      } catch (error) {
        assert.equal(error.code, 'EADDRINUSE');
      }
    }
    await assert.rejects(availablePortBlock(block), /no available E2E port block/, 'B5');
  } finally {
    await Promise.all(blockers.map(owner => new Promise(resolve => owner.close(resolve))));
  }

  const foreign = net.createServer();
  await new Promise((resolve, reject) => {
    foreign.once('error', reject);
    foreign.listen(0, '127.0.0.1', resolve);
  });
  const occupiedPort = foreign.address().port;
  try {
    const selected = await availablePort(occupiedPort);
    assert.notEqual(selected, occupiedPort, 'P3 falls back');
    assert.ok(foreign.listening, 'P3 preserves the foreign listener');
    await waitForPort(occupiedPort, 1_000);
    await assert.rejects(availablePort(-1), { code: 'ERR_SOCKET_BAD_PORT' }, 'P4');
    // P6 foreign listener + real ScenarioHost bind failure -> reject readiness,
    // send no business request, preserve the foreign owner. This negative
    // control failed with TCP-only readiness (which returned before bind failed).
    const collision = spawnServer('echo', occupiedPort);
    try {
      await assert.rejects(waitForPort(occupiedPort, 30_000, collision.server), /server exited before it listened/u, 'P6');
      assert.ok(foreign.listening, 'P6 foreign listener survives');
    } finally {
      await stopServer(collision.server);
    }

  } finally {
    await new Promise((resolve, reject) => foreign.close(error => error ? reject(error) : resolve()));
  }
  assert.equal(await availablePort(occupiedPort), occupiedPort, 'P5 reuses after close');

  // Child-announcement decision table: A1 socket + complete exact stderr line
  // -> ready; A2 socket + absent/wrong/stdout-only/partial line -> deadline;
  // A3 exact line without socket -> deadline; A4 A2 partial + newline -> ready.
  // IPC orders the observations without startup sleeps. These synthetic
  // children validate the harness protocol; P6 above checks the real bind owner.
  for (const [rule, listens, emission] of [
    ['A1', true, 'exact'],
    ['A2 absent', true, 'absent'],
    ['A2 wrong', true, 'wrong'],
    ['A2 stdout', true, 'stdout'],
    ['A2 partial', true, 'partial'],
    ['A3', false, 'exact'],
  ]) {
    const port = await availablePort(0);
    const line = `awaken-coordinator listening on http://127.0.0.1:${port}`;
    const child = spawn(process.execPath, ['--input-type=module', '-e', String.raw`
      import net from 'node:net';
      const line = ${JSON.stringify(line)};
      const listener = net.createServer();
      const announce = () => {
        const emission = ${JSON.stringify(emission)};
        if (emission === 'exact') process.stderr.write(line + '\n');
        if (emission === 'wrong') process.stderr.write(line + '/wrong\n');
        if (emission === 'stdout') process.stdout.write(line + '\n');
        if (emission === 'partial') process.stderr.write(line);
        process.send('announced');
      };
      process.on('message', () => process.stderr.write('\n'));
      process.stdin.on('end', () => process.exit(0));
      process.stdin.resume();
      if (${listens}) listener.listen(${port}, '127.0.0.1', announce);
      else announce();
    `], { stdio: ['pipe', 'ignore', 'pipe', 'ipc'] });
    trackSpawnedServer(port, child, line);
    try {
      await Promise.race([
        once(child, 'message'),
        once(child, 'exit').then(([code, signal]) => { throw new Error(`${rule}: child exited ${code}/${signal} before announcement`); }),
      ]);
      if (rule === 'A1') await waitForPort(port, 1_000, child);
      else await assert.rejects(waitForPort(port, 50, child), /server did not listen/u, rule);
      if (emission === 'partial') {
        child.send('commit newline');
        await waitForPort(port, 1_000, child);
      }
    } finally {
      await stopServer(child);
    }
  }

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

  // Receipt-read failure table: C1 the non-idempotent send already returned its
  // exact receipt; C2 the next idempotent history read may fail transiently;
  // C3 the caller explicitly classifies that failure; C4 a permanent failure is
  // not classified. Effects: E1 C1+C2+C3 retries list without replaying send;
  // E2 C1+C4 surfaces the original error immediately. Rules W7 and W8 keep the
  // retry authority with the failover scenario rather than changing the shared
  // waiter's fail-closed default.
  let transientReads = 0;
  const transient = Object.assign(new Error('projection sources changed'), { status: 503 });
  const transientClient = {
    beta: {
      sessions: {
        events: {
          list: async function* list() {
            transientReads += 1;
            if (transientReads === 1) throw transient;
            yield* snapshots.at(-1);
          },
        },
      },
    },
  };
  const afterTransient = await waitForSessionEventReceipt(
    transientClient,
    'session',
    'receipt',
    undefined,
    ({ delta }) => delta.some((event) => event.id === 'new-idle'),
    'W7 retry classified history read only',
    { timeoutMs: 1_000, pollMs: 1, retryReadError: (error) => error?.status === 503 },
  );
  assert.equal(transientReads, 2, 'W7 retries exactly the failed idempotent list');
  assert.equal(afterTransient.receiptEvent.id, 'receipt', 'W7 retains the exact receipt scope');

  let permanentReads = 0;
  const permanent = Object.assign(new Error('invalid request'), { status: 400 });
  const permanentClient = {
    beta: {
      sessions: {
        events: {
          list: async function* list() {
            permanentReads += 1;
            throw permanent;
          },
        },
      },
    },
  };
  await assert.rejects(
    waitForSessionEventReceipt(
      permanentClient,
      'session',
      'receipt',
      undefined,
      () => true,
      'W8 permanent history error',
      { timeoutMs: 1_000, pollMs: 1, retryReadError: (error) => error?.status === 503 },
    ),
    (error) => error === permanent,
  );
  assert.equal(permanentReads, 1, 'W8 preserves fail-closed default for unclassified errors');

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

// Keep incomplete asynchronous test execution from exiting successfully.
await main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
