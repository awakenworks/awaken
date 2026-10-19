import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { randomBytes } from 'node:crypto';
import { spawn } from 'node:child_process';
import { cargoExecutable, WORKER_BIN_ENV, requirePrebuiltExecutable } from '../cargo_binary.mjs';
import { availablePort, hardKillServer, stopServer, runCleanupSteps, waitForValue } from '../harness.mjs';
import { withoutControlAuthority } from '../process_environment.mjs';
import { sqliteRows } from '../sqlite.mjs';
import { installHostResourceFixture } from './trusted_host_resources.mjs';
import { createTlsIdentityFixture } from './tls_identity_fixture.mjs';
import { startTlsProxy, stopTlsProxy } from './tls_proxy_fixture.mjs';
import { workdirWorkerManifestFixture } from './worker_manifest_fixture.mjs';

// The registry retains historical incarnations. Observe its current projection
// with the same max-generation rule as worker-registry codec::current_workers_query;
// neither expired history nor insertion order selects the active registration.
export const CURRENT_WORKER_ROWS_QUERY = `SELECT worker_id, incarnation_id, generation,
  state, expires_at_ms, manifest_json, credential_observations_json, observation_sequence
  FROM worker_registry_worker AS current
  WHERE generation = (SELECT MAX(generation) FROM worker_registry_worker AS history
    WHERE history.worker_id = current.worker_id) ORDER BY worker_id`;

export function standaloneWorkerCargoFeatures(scopedHost) {
  // Scoped Repository I/O belongs to the companion's fixture build. Native
  // Local I/O belongs to the Worker's execution composition, not Host storage.
  return scopedHost ? [] : ['awaken-execution-composition/test-support'];
}

export function standaloneWorkerSandboxConfig(providerCredentials, runtime) {
  if (providerCredentials?.sandboxFields) {
    return Object.entries(providerCredentials.sandboxFields)
      .filter(([, value]) => value !== undefined)
      .map(([field, value]) => `${field} = ${JSON.stringify(value)}`);
  }
  return [
    `sandbox_tier = ${JSON.stringify(providerCredentials?.sandboxTier ?? 'local')}`,
    ...(providerCredentials?.image
      ? [`container_image = ${JSON.stringify(providerCredentials.image)}`]
      : providerCredentials ? [] : ['sandbox_trusted_host_enabled = true']),
    `acp_clis = ${JSON.stringify(runtime === 'native' ? [] : [runtime])}`,
  ];
}

// This fixture owns only the split-process deployment of the EXISTING resource
// oracle. Control creates the WorkerLocal source through normal startup, then
// runs with acp_clis=[]; the production Worker artifact is the only ACP claimer.
// No scenario router, synthetic dispatch, source-row write or second resource
// implementation is involved. The publisher here is an explicit trusted test
// operator, NOT evidence of automatic authenticated projection delivery.
export async function prepareStandaloneHostWorker({ root, temp, productDirectory, runtime, scopedHost,
  providerCredentials, registryPath, readRegistry, supervisor = 'process' }) {
  assert.ok(['process', 'systemd'].includes(supervisor), 'known Worker supervisor');
  assert.ok(supervisor !== 'systemd' || providerCredentials, 'systemd requires the frozen Provider bundle');
  const workerId = `resource-${runtime}-worker`;
  const workerData = path.join(temp, 'standalone worker');
  const internalPort = await availablePort(0);
  const tlsPort = await availablePort(0);
  const adminPort = await availablePort(0);
  const identity = createTlsIdentityFixture(path.join(temp, 'tls'), {
    caCommonName: 'Awaken standalone resource fixture', serverCommonName: '127.0.0.1',
    subjectAltNames: ['IP:127.0.0.1'],
  });
  const signing = (id) => ({ worker_id: id, key_id: `${id}-key`,
    credential_id: `${id}-request`, secret_base64: randomBytes(32).toString('base64') });
  const embeddedSigning = signing('embedded-resource-worker');
  const workerSigning = signing(workerId);
  const embeddedFile = path.join(temp, 'embedded-signer.json');
  const workerFile = path.join(temp, 'worker-signer.json');
  const trustFile = path.join(temp, 'worker-trust.json');
  fs.writeFileSync(embeddedFile, JSON.stringify(embeddedSigning), { mode: 0o600 });
  fs.writeFileSync(workerFile, JSON.stringify(workerSigning), { mode: 0o600 });
  fs.writeFileSync(trustFile, JSON.stringify([embeddedSigning, workerSigning]), { mode: 0o600 });
  const bindingFile = path.join(temp, 'local-bindings.json');
  const envelopeFile = path.join(temp, 'recipient-keys.json');
  if (providerCredentials) {
    // D1 normal Provider authoring crosses the real Vault issuer. Only transport
    // keys are fixture-projected; Provider material is NEVER written here.
    // D2 independent Native and ACP holders receive distinct random keys.
    const keys = [
      ['worker', 'awaken.worker'],
      ['workload', 'awaken.workload.acp'],
    ].map(([boundary, trust_domain]) => ({id:boundary,holder:{boundary,trust_domain},
      key_base64:randomBytes(32).toString('base64')}));
    fs.writeFileSync(envelopeFile, JSON.stringify({ttl_seconds:3600,keys}), {mode:0o600});
  }
  const workerConfig = path.join(temp, 'standalone-worker.toml');
  fs.writeFileSync(workerConfig, [
    `data_dir = ${JSON.stringify(workerData)}`,
    `sandbox_dir = ${JSON.stringify(path.join(workerData, 'sandboxes'))}`,
    `worker_id = ${JSON.stringify(workerId)}`,
    `worker_request_credential_file = ${JSON.stringify(workerFile)}`,
    `worker_server_ca_certificate_file = ${JSON.stringify(identity.caCertificate)}`,
    ...(providerCredentials
      ? [`credential_envelope_keys_file = ${JSON.stringify(envelopeFile)}`]
      : [`worker_local_credential_bindings_file = ${JSON.stringify(bindingFile)}`]),
    `worker_admin_listen = "127.0.0.1:${adminPort}"`,
    'worker_credential_probe_interval_secs = 1',
    // Keep the production 30s evidence window: a 5s lease is shorter than
    // the Coordinator-owned heartbeat publication budget and is now rejected
    // during startup rather than creating intermittent eligibility gaps.
    'worker_credential_observation_ttl_secs = 30',
    ...standaloneWorkerSandboxConfig(providerCredentials, runtime),
    `sandbox_trusted_host_scoped_execution = ${scopedHost}`,
    // Deterministic peers own their stderr. Preserve assertion failures in the
    // Worker log instead of reducing a crashed peer to an unexplained EOF.
    ...(!providerCredentials ? ['sandbox_inherit_agent_stderr = true'] : []),
  ].join('\n'));
  const environment = providerCredentials
    ? {...providerCredentials.environment, HOME:workerData, AWAKEN_DATA_DIR:workerData}
    : await installHostResourceFixture(root, path.join(temp, 'worker fixture'), workerData, runtime);
  fs.mkdirSync(workerData,{recursive:true});
  if (providerCredentials?.wrapperCache) fs.cpSync(providerCredentials.wrapperCache,
    path.join(workerData,'acp-wrappers','claude'),{recursive:true,verbatimSymlinks:true});
  // Provider journey requires the caller's frozen bundle. This path must never
  // override Cargo target/build directories or silently rebuild another revision.
  const binary = providerCredentials ? requirePrebuiltExecutable(WORKER_BIN_ENV) : cargoExecutable({
    cwd: root, packageName: 'awaken-worker', targetName: 'awaken-worker',
    features: standaloneWorkerCargoFeatures(scopedHost),
    snapshotDestination: path.join(productDirectory,
      process.platform === 'win32' ? 'awaken-worker.exe' : 'awaken-worker'),
  });
  assert.ok(binary, "standalone Provider journey requires AWAKEN_E2E_WORKER_BIN");
  let worker;
  let proxy;
  let output = '';
  let reference;
  let generation = 0;
  let expectedRestarts = 0;
  const logFile = path.join(temp, 'standalone-worker.log');
  const args = ['--config', workerConfig, '--server', `https://127.0.0.1:${tlsPort}`];
  const childEnv = { ...withoutControlAuthority(environment), AWAKEN_E2E_SHUTDOWN_ON_STDIN_EOF: providerCredentials ? '0' : '1' };
  const supervised = supervisor === 'systemd'
    ? (await import('./systemd_worker_supervisor.mjs')).systemdWorkerSupervisor({
      directory: temp, binary, args, cwd: root, environment: childEnv, logFile,
    }) : undefined;
  const logs = () => supervised && fs.existsSync(logFile) ? fs.readFileSync(logFile, 'utf8') : output;
  const registry = registryPath ?? path.join(temp, 'data', 'worker-registry.db');
  const rows = readRegistry ?? (() => sqliteRows(registry,
    CURRENT_WORKER_ROWS_QUERY));
  const current = () => rows().find((row) => row.worker_id === workerId);
  const publish = (value) => {
    // One atomic replacement in the protected fixture directory. Never expose
    // a partially written JSON file to the Worker's asynchronous probe.
    const next = bindingFile + '.next';
    fs.writeFileSync(next, JSON.stringify(value), { mode: 0o600 });
    fs.renameSync(next, bindingFile);
  };
  const binding = (enabled = true) => ({ credential: reference, cli_id: runtime, enabled });
  const hasExactCredential = (row) => JSON.parse(row.credential_observations_json).some((item) =>
      item.credential.id === reference.id && item.credential.revision === reference.revision
      && item.state === 'available' && item.observed_at_ms <= Date.now()
      && item.valid_until_ms > Date.now());
  const exactAvailable = (row) => row && row.state === 'ready' && row.expires_at_ms > Date.now()
    && (providerCredentials || hasExactCredential(row));
  const assertCustody = () => {
    for (const forbidden of ['credential.db', 'files.db', 'memory_fs.db', 'resources.db',
      'sessions.db', 'worker-registry.db', 'control-seal.key']) {
      assert.equal(fs.existsSync(path.join(workerData, forbidden)), false,
        `standalone Worker must not create ${forbidden}`);
    }
    assert.ok(!logs().includes(workerSigning.secret_base64), 'request signer is not logged');
  };
  const waitReady = async () => {
    // W1 cause/effect table: old embedded lease still live + exact standalone
    // Ready => wait; expired old lease + exact standalone Ready as the sole ACP
    // claimer => admit; a renewed embedded/foreign ACP claimer or missing new
    // Worker => bounded failure. The read is one Registry projection, and no
    // fixture writes or deletes a Worker row to make this condition true.
    const observation = await waitForValue(async () => {
      if (supervised) assert.notEqual(supervised.status().ActiveState, 'failed', logs());
      else assert.ok(worker.exitCode === null && worker.signalCode === null,
        `production Worker exited during registration: ${output}`);
      const currentRows = rows();
      const row = currentRows.find((value) => value.worker_id === workerId);
      const eligibleWorkerIds = currentRows.filter((value) => value.state === 'ready'
        && value.expires_at_ms > Date.now()
        && JSON.parse(value.manifest_json).capabilities.includes(runtime === 'native' ? 'native-runtime' : `acp:${runtime}`))
        .map((value) => value.worker_id);
      return { generation: row?.generation, available: exactAvailable(row), eligibleWorkerIds };
    }, (value) => value.generation > generation && value.available
      && value.eligibleWorkerIds.length === 1 && value.eligibleWorkerIds[0] === workerId,
    'W1 fresh exact standalone Worker is the sole eligible ACP claimer', { timeoutMs: 120_000 });
    generation = observation.generation;
    assertCustody();
  };
  const launch = async () => {
    if (supervised) await supervised.start();
    else {
      worker = spawn(binary, args, {
        cwd: root, env: childEnv,
        // Physical signal acceptance must run the production signal owner.
        stdio: [providerCredentials ? 'ignore' : 'pipe', 'pipe', 'pipe'],
      });
      const capture = (chunk) => {
        output += chunk.toString();
        fs.appendFileSync(logFile, chunk, {mode:0o600});
      };
      worker.stdout.on('data', capture);
      worker.stderr.on('data', capture);
    }
    await waitReady();
    if (supervised) assert.equal(supervised.status().restarts, 0,
      'Ready must not conceal a failed initial incarnation');
  };
  const controlFields = {
    internal_bind:`127.0.0.1:${internalPort}`, worker_id:embeddedSigning.worker_id,
    worker_request_credential_file:embeddedFile, worker_trust_credentials_file:trustFile,
    ...(providerCredentials ? {run_local_pool:false,credential_envelope_keys_file:envelopeFile} : {}),
  };
  return {
    physicalRoot: workerData,
    controlFields,
    controlConfig: Object.entries(controlFields).map(([key,value]) => `${key} = ${JSON.stringify(value)}`),
    get process() { return worker; },
    get pid() { return supervised ? supervised.status().pid : worker?.pid; },
    current,
    async replace(signal) {
      assert.ok(!supervised, 'explicit process replacement is separate from systemd recovery');
      assert.ok(['SIGKILL','SIGTERM'].includes(signal));
      if (signal === 'SIGKILL') await hardKillServer(worker, 'standalone Cowork Worker');
      else {
        const {once} = await import('node:events');
        const exited = once(worker,'exit',{signal:AbortSignal.timeout(90_000)});
        assert.ok(worker.kill(signal)); await exited;
        assert.equal(worker.signalCode,null); assert.equal(worker.exitCode,0);
      }
      const stopped = {exit_code:worker.exitCode,signal:worker.signalCode};
      await launch();
      return stopped;
    },
    async start(source) {
      if (!providerCredentials) {
        reference = { id: source.id, revision: source.revision };
        publish([binding()]);
      }
      // W0: valid registration bytes + bare claimed identity => HTTP 401 from
      // the production signed authenticator; TLS alone is not authorization.
      const denied = await fetch(`http://127.0.0.1:${internalPort}/v1/worker/register`, {
        method: 'POST', headers: { 'content-type': 'application/json', 'x-awaken-worker-id': workerId },
        body: JSON.stringify({ registration: { worker_id: workerId, incarnation_id: 'unsigned',
          manifest: workdirWorkerManifestFixture({ buildDigest: 'unsigned', capabilities: [] }) } }),
      });
      assert.equal(denied.status, 401, 'W0 a bare Worker identity cannot register');
      proxy = await startTlsProxy(tlsPort, internalPort, identity.serverCertificate, identity.serverKey);
      await launch();
    },
    async verifySupervisorRecovery() {
      assert.ok(supervised, 'requires external systemd supervision');
      // A Registry receipt can precede final pool assembly. Pause only after
      // the installed admin owner confirms actual claim admission, not merely
      // after TCP bind or a registry row, so this is a RUNNING authority fault.
      await waitForValue(async () => {
        try { return (await fetch(`http://127.0.0.1:${adminPort}/readyz`,
          {signal:AbortSignal.timeout(1000)})).status; } catch { return undefined; }
      }, status => status === 200, 'SR0 Worker pool accepting before physical pause', {timeoutMs:30_000});
      // SR1 Ready (idle or an existing caller-owned journey barrier) + real OS
      // stop beyond the registered authority deadline
      // -> resumed old process fails closed and exits; systemd alone starts its
      // replacement. SR2 new PID/InvocationID + exactly one restart + newer Ready
      // registry generation; credentials/root remain the same, no DB mutation.
      // This is a physical process-pause fault, not a remote-host outage or a
      // diagnosis of historical Podman probe latency under model load.
      const before = supervised.status();
      assert.equal(before.restarts, expectedRestarts, 'no hidden startup restart');
      const registered = current();
      const logOffset = logs().length;
      console.log(`Standalone systemd pause: ${JSON.stringify({unit:supervised.name,
        pid:before.pid, generation, expires_at_ms:registered.expires_at_ms})}`);
      process.kill(before.pid, 'SIGSTOP');
      try {
        await new Promise(resolve => setTimeout(resolve, Math.max(0, registered.expires_at_ms - Date.now()) + 2000));
      } finally { process.kill(before.pid, 'SIGCONT'); }
      await waitReady();
      const after = supervised.status();
      expectedRestarts = before.restarts + 1;
      assert.equal(after.restarts, expectedRestarts, 'one manager restart');
      assert.notEqual(after.pid, before.pid);
      assert.notEqual(after.InvocationID, before.InvocationID);
      const exited = supervised.exits().filter(row => row.invocation === before.InvocationID);
      assert.equal(exited.length, 1, 'one old incarnation exit observation');
      assert.equal(exited[0].code, 'exited');
      assert.notEqual(exited[0].status, '0');
      assert.equal(exited[0].result, 'exit-code');
      assert.ok(!fs.existsSync(`/proc/${before.pid}`), 'old process has physically exited');
      assert.match(logs().slice(logOffset), /worker lost execution authority; supervisor restart required/);
      console.log(`Standalone systemd recovery: ${JSON.stringify({unit:supervised.name,
        old_pid:before.pid, pid:after.pid, old_generation:registered.generation,
        generation, restarts:after.restarts})}`);
      return { exit_code:Number(exited[0].status), signal:null,
        supervisor:supervised.name, old_invocation:before.InvocationID,
        invocation:after.InvocationID, restarts_before:before.restarts,
        restarts_after:after.restarts };
    },
    async verifyRevocation() {
      // Cause/effect rules: W2 same live process + disabled/absent projection
      // => a NEW observation sequence contains no usable exact reference;
      // W3 restoration => same generation becomes available again. Control's
      // source remains active and unchanged, so no other login can mask denial.
      // Race during probe/cancellation belongs to AcpProjectedBindingUse and
      // its Rust decision tables, not to timing guesses in this process E2E.
      for (const remove of [false, true]) {
        const before = current();
        if (remove) fs.unlinkSync(bindingFile);
        else publish([binding(false)]);
        await waitForValue(async () => current(), (row) =>
          row.generation === generation && row.state === 'ready' && row.expires_at_ms > Date.now()
          && row.observation_sequence > before.observation_sequence && !hasExactCredential(row),
        'W2 standalone revocation heartbeat', { timeoutMs: 30_000 });
        publish([binding()]);
        await waitForValue(async () => current(), (row) =>
          row.generation === generation && exactAvailable(row),
        'W3 same-process restored projection', { timeoutMs: 30_000 });
      }
      assertCustody();
    },
    async restart({ crash = false } = {}) {
      assert.ok(!supervised, 'explicit process replacement is separate from systemd recovery');
      // W4 dirty live Sessions + only Worker replacement => a strictly newer
      // registered generation recovers the SAME resource edits. Control stays
      // alive; the shared resource oracle rejects recloning/cleanup resurrection.
      if (crash) await hardKillServer(worker, 'standalone resource Worker');
      else {
        await stopServer(worker);
        assert.equal(worker.exitCode, 0, `W4 stdin EOF drained the Worker: ${output}`);
      }
      await launch();
    },
    assertCustody,
    async cleanup() {
      await runCleanupSteps([
        async () => {
          if (supervised) {
            const before = supervised.status();
            await supervised.stop();
            assert.equal(before.restarts, expectedRestarts, 'unexpected later Worker restart');
            assertCustody();
            console.log(`Standalone systemd clean shutdown: ${JSON.stringify({
              unit:supervised.name, generation, restarts:expectedRestarts, exit_code:0,
            })}`);
          } else if (worker) await stopServer(worker, { requireCleanExit: true });
        },
        () => supervised?.dispose(),
        () => proxy && stopTlsProxy(proxy),
      ]);
      if (worker) console.log(`Standalone Worker clean shutdown: ${JSON.stringify({
        worker_id:workerId, generation, exit_code:worker.exitCode, signal:worker.signalCode,
      })}`);
    },
  };
}
