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
  retryableReadStatus,
  scenarioMemoryStore,
  spawnServer,
  stopServer,
  runCleanupSteps,
  trackSpawnedServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

async function main() {
  // Shutdown cause/effect graph: live-at-request × stdin pipe × observed
  // terminal (0/nonzero/signal) × strict mode -> acceptance or failure.
  // S1 live + graceful 0 => pass (EOF and Unix SIGINT); S2 live + nonzero =>
  // fail; S3 already terminal => strict failure EVEN for 0, but default cleanup
  // stays idempotent for deliberate faults. IPC proves handlers exist before
  // stopping; exact child handles and finally cleanup prevent orphan fixtures.
  for (const piped of process.platform === 'win32' ? [true] : [true, false]) {
    for (const code of [0, 7]) {
      const child = spawn(process.execPath, ['-e', `
        const finish = () => process.exit(${code});
        process.on('SIGINT', finish);
        process.stdin.on('end', finish);
        process.stdin.resume();
        setInterval(() => {}, 1000);
        process.send('ready');
      `], {stdio:[piped ? 'pipe' : 'ignore', 'ignore', 'ignore', 'ipc']});
      try {
        await once(child, 'message', {signal:AbortSignal.timeout(5000)});
        const stopped = stopServer(child, {requireCleanExit:true});
        if (code === 0) await stopped;
        else await assert.rejects(stopped, /live_before=true; code=7; signal=null/, 'S2');
        assert.equal(child.exitCode, code, 'S1/S2 actual OS terminal outcome');
      } finally { await stopServer(child); }
    }
  }
  for (const code of [0, 7, 'SIGKILL']) {
    const child = spawn(process.execPath, ['-e', code === 'SIGKILL'
      ? `process.kill(process.pid, 'SIGKILL')` : `process.exit(${code})`], {stdio:'ignore'});
    await once(child, 'exit');
    await assert.rejects(stopServer(child, {requireCleanExit:true}), /live_before=false/, 'S3');
    await stopServer(child);
  }
  // S4 Unix parent exits 0 while a descendant still owns stdout -> strict
  // shutdown remains pending until the last diagnostic bytes and close event.
  // fd 4 is an exact release barrier; this is not a timing-based log guess.
  if (process.platform !== 'win32') {
    const descendant = `
      const input = new (require('node:net').Socket)({fd:3});
      input.once('data', () => process.stdout.write('last diagnostic', () => process.exit(0)));
      input.once('end', () => process.exit(0));
      setTimeout(() => process.exit(9), 5000).unref();
      process.send('ready');
    `;
    const child = spawn(process.execPath, ['-e', `
      const nested = require('node:child_process').spawn(process.execPath, ['-e', ${JSON.stringify(descendant)}],
        {stdio:['ignore',1,'ignore',4,'ipc']});
      nested.once('message', () => process.send('ready'));
      process.stdin.on('end', () => process.exit(0));
      process.stdin.resume();
    `], {stdio:['pipe','pipe','ignore','ipc','pipe']});
    let output = '';
    child.stdout.on('data', chunk => { output += chunk; });
    try {
      await once(child, 'message', {signal:AbortSignal.timeout(5000)});
      const exited = once(child, 'exit');
      let settled = false;
      const stopped = stopServer(child, {requireCleanExit:true}).then(() => { settled = true; });
      await exited;
      await new Promise(resolve => setImmediate(resolve));
      assert.equal(settled, false, 'S4 process exit alone cannot erase diagnostic evidence');
      child.stdio[4].write('release');
      await stopped;
      assert.equal(output, 'last diagnostic');
    } finally {
      child.stdio[4].end();
      await stopServer(child);
    }
  }
  // C1 original failure × C2-C4 each dependent cleanup succeeds/fails. Enumerate
  // all 16 combinations: EVERY step runs once in order, original error first,
  // all teardown errors preserved, and evidence is erased iff no error exists.
  // Async rejection and synchronous throw are both causes. No parallel cleanup
  // may stop Control/transport before the Worker's drain attempt settles.
  const cleanupRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-cleanup-rules-'));
  try {
    for (let mask = 0; mask < 16; mask++) {
      const evidence = path.join(cleanupRoot, String(mask));
      fs.writeFileSync(evidence, 'diagnostic bytes');
      const errors = Array.from({length:4}, (_, i) => new Error(`cause-${i}`));
      const observed = [];
      let pending = false;
      const steps = [1, 2, 3].map(index => () => {
        assert.equal(pending, false, 'previous cleanup settled');
        observed.push(index);
        if (index === 2 && (mask & (1 << index))) throw errors[index];
        pending = true;
        return new Promise((resolve, reject) => setImmediate(() => {
          pending = false;
          if (mask & (1 << index)) reject(errors[index]);
          else resolve();
        }));
      });
      const cleanup = runCleanupSteps(steps, mask & 1 ? [errors[0]] : [])
        .then(() => fs.unlinkSync(evidence));
      if (mask === 0) await cleanup;
      else await assert.rejects(cleanup, error => {
        assert.ok(error instanceof AggregateError);
        assert.deepEqual(error.errors, errors.filter((_, i) => mask & (1 << i)));
        return true;
      });
      assert.deepEqual(observed, [1,2,3]);
      assert.equal(fs.existsSync(evidence), mask !== 0);
      if (mask) assert.equal(fs.readFileSync(evidence,'utf8'), 'diagnostic bytes');
    }
  } finally { fs.rmSync(cleanupRoot, {recursive:true,force:true}); }

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

  // Stream-first receipt table: C1 a stream can replay an older idle; C2 one
  // receipt changes from pending to processed under the same id; C3 an
  // requires_action idle precedes end_turn; C4 the stream may close before the
  // terminal event and history then contains the missing suffix. Effects: E1
  // same-id replacement preserves one receipt in order; E2 only the caller's
  // post-receipt predicate completes; E3 a complete stream performs no list;
  // E4 a dropped stream performs one authoritative backfill and never re-sends.
  // Rules W9=C1+C2+C3=>E1+E2+E3 and W10=C2+C4=>E1+E2+E4.
  let streamCalls = 0;
  let streamListCalls = 0;
  const streamClient = {
    beta: { sessions: { events: {
      stream: async function* stream() {
        streamCalls += 1;
        yield { id: 'old-idle', type: 'session.status_idle', processed_at: 't0', stop_reason: { type: 'end_turn' } };
        yield { id: 'receipt', type: 'user.message', processed_at: null };
        yield { id: 'receipt', type: 'user.message', processed_at: 't1' };
        yield { id: 'reply', type: 'agent.message', processed_at: 't2' };
        yield { id: 'ask', type: 'session.status_idle', processed_at: 't3', stop_reason: { type: 'requires_action' } };
        yield { id: 'done', type: 'session.status_idle', processed_at: 't4', stop_reason: { type: 'end_turn' } };
      },
      list: async function* list() {
        streamListCalls += 1;
        throw new Error('W9 completed stream must not list history');
      },
    } } },
  };
  const streamed = await waitForSessionEventReceipt(
    streamClient,
    'session',
    'receipt',
    ['managed-agents-test'],
    ({ delta }) => delta.some((event) => event.type === 'session.status_idle'
      && event.stop_reason?.type === 'end_turn'),
    'W9 stream-first exact receipt',
    { timeoutMs: 1_000, preferStream: true },
  );
  assert.equal(streamCalls, 1, 'W9 opens one turn stream');
  assert.equal(streamListCalls, 0, 'W9 needs no polling or backfill');
  assert.deepEqual(
    streamed.events.map((event) => event.id),
    ['old-idle', 'receipt', 'reply', 'ask', 'done'],
    'W9 replaces the pending receipt and ignores the older idle',
  );

  let droppedStreams = 0;
  let backfills = 0;
  const droppedClient = {
    beta: { sessions: { events: {
      stream: async function* stream() {
        droppedStreams += 1;
        // Simulate reconnecting after the receipt frame but before its reply.
        // History must put this live suffix back after the receipt.
        yield { id: 'reply', type: 'agent.message', processed_at: 't2' };
      },
      list: async function* list() {
        backfills += 1;
        yield { id: 'receipt', type: 'user.message', processed_at: 't1' };
        yield { id: 'reply', type: 'agent.message', processed_at: 't2' };
        yield { id: 'done', type: 'session.status_idle', processed_at: 't3', stop_reason: { type: 'end_turn' } };
      },
    } } },
  };
  const backfilled = await waitForSessionEventReceipt(
    droppedClient,
    'session',
    'receipt',
    ['managed-agents-test'],
    ({ delta }) => delta.some((event) => event.id === 'done'),
    'W10 dropped stream history reconciliation',
    { timeoutMs: 1_000, preferStream: true },
  );
  assert.equal(droppedStreams, 1, 'W10 does not reopen after history completes');
  assert.equal(backfills, 1, 'W10 performs one authoritative backfill');
  assert.deepEqual(backfilled.delta.map((event) => event.id), ['reply', 'done']);

  // W10a extends the same read-only recovery rule to the actual undici error
  // seen when a response body socket closes: C1 exact receipt accepted; C2
  // either stream or history read drops before terminal; C3 first history can
  // still be pending; C4 next stream commits the receipt and reply. Effects:
  // C1-C4 => one backfill, one reconnect, one receipt and zero send replays.
  const rawSocketError = Object.assign(new TypeError('terminated'), {
    cause: Object.assign(new Error('other side closed'), { code: 'UND_ERR_SOCKET' }),
  });
  const pendingReceipt = { id: 'receipt', type: 'user.message', processed_at: null };
  const committedReceipt = { ...pendingReceipt, processed_at: 't1' };
  const reply = { id: 'reply', type: 'agent.message', processed_at: 't2' };
  const done = { id: 'done', type: 'session.status_idle', processed_at: 't3', stop_reason: { type: 'end_turn' } };
  for (const droppedRead of ['stream', 'history']) {
    let streams = 0;
    let histories = 0;
    let sends = 0;
    const client = { beta: { sessions: { events: {
      send: async () => { sends += 1; throw new Error('accepted command must not be replayed'); },
      stream: async function* stream() {
        streams += 1;
        if (streams === 1) {
          yield pendingReceipt;
          if (droppedRead === 'stream') throw rawSocketError;
          return;
        }
        yield committedReceipt;
        yield reply;
        yield done;
      },
      list: async function* list() {
        histories += 1;
        if (droppedRead === 'history') throw rawSocketError;
        yield pendingReceipt;
      },
    } } } };
    const observed = await waitForSessionEventReceipt(
      client, 'session', 'receipt', undefined,
      ({ delta }) => delta.some((event) => event.id === 'done'),
      `W10a ${droppedRead} socket drop`,
      { timeoutMs: 1_000, pollMs: 1, preferStream: true,
        retryReadError: (error) => retryableReadStatus(error) !== null },
    );
    assert.equal(streams, 2, `W10a ${droppedRead} reconnects the read stream`);
    assert.equal(histories, 1, `W10a ${droppedRead} reconciles the durable history`);
    assert.equal(sends, 0, `W10a ${droppedRead} never replays the accepted command`);
    assert.equal(observed.receiptEvent?.processed_at, 't1');
    assert.equal(observed.events.filter((event) => event.id === 'receipt').length, 1);
    assert.deepEqual(observed.delta, [reply, done]);
  }

  // Receipt-read failure table: C1 the unkeyed send already returned its exact
  // receipt; C2 the next idempotent history read may fail with a classified
  // HTTP/transport transient; C3 history may fail with an unclassified product
  // error; C4 classified failures may persist to the deadline. Effects: E1
  // C1+C2 retries list without replaying send; E2 C1+C3 surfaces the original
  // error immediately; E3 C1+C4 surfaces the last exact transient after bounded
  // observation. Constraints: only the read callback is inside waitForValue;
  // the retry classifier is never command authorization. Decision rules W7,
  // W8 and W9 cover transient recovery, permanent failure, and exhaustion.
  const retryableStatuses = [408, 409, 429, 500, 502, 503, 504];
  for (const status of retryableStatuses) {
    assert.equal(retryableReadStatus({ status }), status, `W7 classifies HTTP ${status}`);
  }
  for (const name of ['APIConnectionError', 'APIConnectionTimeoutError']) {
    assert.equal(retryableReadStatus({ name }), name, `W7 classifies SDK ${name}`);
  }
  assert.equal(retryableReadStatus(rawSocketError), 'UND_ERR_SOCKET', 'W10a classifies a dropped read body');
  const permanentReadErrors = [
    { status: 400 },
    { status: 401 },
    { status: 403 },
    { status: 404 },
    { status: 422 },
    new Error('product invariant'),
    new TypeError('bad decoder'),
  ];
  for (const error of permanentReadErrors) {
    assert.equal(
      retryableReadStatus(error),
      null,
      'W8 rejects an unclassified read failure',
    );
  }
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
    { timeoutMs: 1_000, pollMs: 1, retryReadError: (error) => retryableReadStatus(error) !== null },
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
      { timeoutMs: 1_000, pollMs: 1, retryReadError: (error) => retryableReadStatus(error) !== null },
    ),
    (error) => error === permanent,
  );
  assert.equal(permanentReads, 1, 'W8 preserves fail-closed default for unclassified errors');

  let exhaustedReads = 0;
  const exhausted = Object.assign(new Error('projection sources keep changing'), { status: 503 });
  const exhaustedClient = {
    beta: {
      sessions: {
        events: {
          list: async function* list() {
            exhaustedReads += 1;
            throw exhausted;
          },
        },
      },
    },
  };
  await assert.rejects(
    waitForSessionEventReceipt(
      exhaustedClient,
      'session',
      'receipt',
      undefined,
      () => true,
      'W9 bounded transient history error',
      {
        timeoutMs: 5,
        pollMs: 1,
        retryReadError: (error) => retryableReadStatus(error) !== null,
      },
    ),
    (error) => error === exhausted,
  );
  assert.ok(exhaustedReads >= 1, 'W9 attempts the idempotent read before surfacing it');

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
