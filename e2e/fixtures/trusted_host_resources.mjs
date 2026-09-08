import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { pathToFileURL } from 'node:url';
import { toFile } from '@anthropic-ai/sdk';
import { allowManagedToolBoundaries, waitForValue } from '../harness.mjs';
import { sqliteScalar } from '../sqlite.mjs';

export async function installHostResourceFixture(root, temp, data, runtime) {
  const { ACP_RUNTIME_IMAGE_CONTRACT } = await import('../acp_runtime_profiles.mjs');
  const row = ACP_RUNTIME_IMAGE_CONTRACT.runtimes.find((entry) => entry.id === runtime);
  assert.ok(row);
  const packagePin = row.requirements.find((entry) =>
    entry.manager === 'npm' && entry.requirement.startsWith('@agentclientprotocol/')).requirement;
  const split = packagePin.lastIndexOf('@');
  const prefix = path.join(data, 'acp-wrappers', runtime);
  const bin = path.join(temp, 'bin');
  fs.mkdirSync(bin, { recursive: true });
  fs.mkdirSync(path.join(prefix, 'node_modules', '.bin'), { recursive: true });
  const suffix = process.platform === 'win32' ? '.exe' : '';
  const executable = path.join(bin, runtime + suffix);
  execFileSync('rustc', [
    '--edition=2024', path.join(root, 'e2e/fixtures/acp_host_fixture_launcher.rs'), '-o', executable,
  ], { env: { ...process.env,
    AWAKEN_FIXTURE_NODE: process.execPath,
    AWAKEN_FIXTURE_SCRIPT: path.join(root, 'e2e/fixtures/acp_host_resource_fixture.mjs'),
  } });
  const wrapperName = row.executables.find((name) => name !== row.discovery.version.executable);
  assert.ok(wrapperName, 'the catalog names the ACP adapter executable');
  const wrapper = path.join(prefix, 'node_modules', '.bin', wrapperName);
  fs.copyFileSync(executable, wrapper);
  if (suffix) fs.copyFileSync(executable, wrapper + suffix);
  fs.writeFileSync(path.join(prefix, 'package.json'), JSON.stringify({
    dependencies: { [packagePin.slice(0, split)]: packagePin.slice(split + 1) },
  }));
  return { ...process.env, PATH: [bin, process.env.PATH].join(path.delimiter) };
}

export async function verifyAgentResources({ client, request, temp, runtime, restart, nativeModel, nativeIsolation = 'namespace', requireScopedExecution = false, expectedPhysicalRoot }) {
  // Cause/effect graph:
  // C1 API binds trusted_host version 1; C2 cwd={default,repo/src};
  // C3 File+Memory+Repository; C4 two Sessions share a MemoryStore and host
  // identity; C5 Worker restart BEFORE Session release. E1 actual file I/O follows rendered
  // paths; E2 concurrent distinct writes persist; E3 remote Git is unchanged;
  // E4 restart retains both writes. Rules R1=C1+C2+C3=>E1; R2=R1+C4=>E2+E3;
  // R3=R2+C5=>E4. Cloud+TrustedHost and invalid policy => reject before Session.
  // The live adapter and deterministic peer run this SAME mission and oracle.
  // R4 independent random File/Memory/Repo bytes + exact HEAD must ALL contribute
  // to the output; apply the exported patch to a fresh checkout as a separate
  // oracle. A static seed, guessed HEAD or string-shaped invalid patch fails.
  // N1 Native reuses the SAME resources, receipt and independent output oracle.
  // Its Bash Hand changes cwd explicitly: Native has no ACP session cwd option.
  // This covers default/nested TOOL cwd, not an invented Native Agent setting.
  // R6 registered Worker copy projections capture the same two heads. On the
  // last receipt both Agents read those bases, then write identical/different
  // bytes at shared paths. Concurrent release => one version per path; conflict
  // never overwrites its winner, including after restart/reclamation retry.
  // S1-S6 Linux scoped x Codex/Claude/Native x default/nested cwd: EACH
  // resource receipt must export its actual PID namespace plus this attempt.
  // Same-as-driver, absent/stale proof or an unconfigured boundary fails.
  // The same oracle runs again after graceful and abrupt Worker replacement
  // (all-in-one OR standalone; the caller owns that process fault boundary).
  // Fixture callers use synthetic inference/credentials; host-login uses the
  // real discovered login with the SAME oracle. Neither is cross-host failover.
  const native = runtime === 'native';
  assert.ok(['namespace', 'workdir'].includes(nativeIsolation));
  const trustedHost = !native || nativeIsolation === 'workdir';
  assert.ok(!requireScopedExecution || (process.platform === 'linux' && trustedHost));
  const parentNamespace = requireScopedExecution ? fs.readlinkSync('/proc/self/ns/pid') : undefined;
  assert.equal(native, typeof nativeModel === 'string', 'Native needs an explicit published model');
  const betas = ['managed-agents-2026-04-01'];
  const memoryHeaders = { 'anthropic-beta': 'agent-memory-2026-07-22' };
  const api = async (method, route, body, expected = 200) => {
    const result = await request(method, route, body);
    assert.equal(result.response.status, expected, `${route}: ${JSON.stringify(result.value)}`);
    return result.value;
  };
  await api('POST', '/v1/awaken/sandbox-execution-policies', {
    id: 'resource-host', config: trustedHost
      ? { isolation: 'workdir', host_access: 'trusted_host' }
      : { isolation: 'namespace' },
  }, 201);
  const environment = await api('POST', '/v1/awaken/environments', {
    command_id: 'resource-host-create', name: 'Resource host',
    config: { type: 'self_hosted' }, sandbox_policy: { policy_id: 'resource-host', version: 1 },
  }, 201);
  assert.deepEqual(environment.sandbox_policy, { policy_id: 'resource-host', version: 1 });
  if (trustedHost) await api('POST', '/v1/awaken/environments', {
    command_id: 'reject-cloud-host', name: 'Rejected cloud',
    config: { type: 'cloud', networking: { type: 'unrestricted' }, packages: {} },
    sandbox_policy: { policy_id: 'resource-host', version: 1 },
  }, 422);
  await api('POST', '/v1/awaken/sandbox-execution-policies', {
    id: 'reject-host-network',
    config: { isolation: 'workdir', host_access: 'trusted_host', network: { mode: 'none' } },
  }, 422);

  const seed = path.join(temp, 'seed repo');
  const remote = path.join(temp, 'remote.git');
  fs.mkdirSync(path.join(seed, 'src'), { recursive: true });
  const git = (args, cwd = seed) => execFileSync('git', args, { cwd, encoding: 'utf8' }).trim();
  git(['init', '-q', '-b', 'main']);
  git(['config', 'user.name', 'Resource Fixture']);
  git(['config', 'user.email', 'fixture@example.invalid']);
  const repoBytes = `REPOSITORY_${randomUUID()}`;
  const memoryBytes = `MEMORY_${randomUUID()}`;
  fs.writeFileSync(path.join(seed, 'README.md'), repoBytes);
  fs.writeFileSync(path.join(seed, 'src', '.keep'), '');
  git(['add', '.']);
  git(['commit', '-qm', 'seed']);
  git(['clone', '-q', '--bare', seed, remote]);
  const remoteHead = git(['rev-parse', 'HEAD'], remote);
  const memory = await client.post('/v1/memory_stores', {
    headers: memoryHeaders, body: { name: 'shared host memory' },
  });
  await client.post(`/v1/memory_stores/${memory.id}/memories`, {
    headers: memoryHeaders, body: { path: '/seed.md', content: memoryBytes },
  });
  const sharedHeads = [];
  for (const sharedPath of ['/shared-a-identical.md', '/shared-z-conflict.md']) {
    sharedHeads.push(await client.post(`/v1/memory_stores/${memory.id}/memories`, {
      headers: memoryHeaders, body: { path: sharedPath, content: memoryBytes },
    }));
  }
  let contention;
  const cases = [];
  for (const [name, cwd] of [['default', undefined], ['nested', 'repo/src']]) {
    const agent = `resource-${runtime}-${name}`;
    await api('PUT', `/v1/config/agents/${agent}`, {
      name: agent, model: native ? { id: nativeModel } : {
        mode: 'backend_default', backend_ref: `acp:${runtime}`,
        ...(cwd ? { configuration: { working_directory: cwd } } : {}),
      },
      system: 'Follow the resource mission exactly. Read resource instructions before file operations.',
      tools: native ? [{ id: 'bash' }] : [],
    });
    await api('POST', `/v1/config/agents/${agent}/publish`);
    const marker = `RESOURCE_BYTES_${runtime}_${name}_${randomUUID()}`;
    const resourceBytes = [marker, memoryBytes, repoBytes, remoteHead].join('\n');
    const file = await client.beta.files.upload({
      file: await toFile(Buffer.from(marker), '资料 brief.txt'), betas,
    });
    const session = await client.beta.sessions.create({
      agent, environment_id: environment.id, betas, resources: [
        { type: 'file', file_id: file.id, mount_path: '/mnt/session/uploads/资料 brief.txt' },
        { type: 'memory_store', memory_store_id: memory.id, instructions:
          'AWK_WORKSPACE={{awaken.workspace}}\nAWK_MEMORY={{awaken.resource.path}}\nAWK_OUTPUTS={{awaken.outputs}}' },
        // file: also disambiguates Windows drive letters from URL schemes.
        { type: 'github_repository', url: pathToFileURL(remote).href, mount_path: '/workspace/repo' },
      ],
    });
    cases.push({ session, marker, name, resourceBytes });
  }
  const execute = async (testCase) => {
    const { session, marker, name, resourceBytes } = testCase;
    const attempt = randomUUID();
    const expected = `${resourceBytes}\n${attempt}`;
    const previous = testCase.attempt;
    testCase.attempt = attempt;
    testCase.expected = expected;
    const receipt = await client.beta.sessions.events.send(session.id, {
      betas, events: [{ type: 'user.message', content: [{ type: 'text', text:
        `AWK_RESOURCE_MISSION\nAWK_RESULT=result-${name}\nAWK_ATTEMPT=${attempt}\nRead the uploaded File using its mounted path, seed.md in Memory, and README.md in the mounted Repository. Their exact bytes are independent secrets not supplied in this user message. Obtain the Repository HEAD using git rev-parse HEAD. Construct exactly five lines: File bytes, Memory seed bytes, Repository README bytes, the HEAD hash, and this message's AWK_ATTEMPT value, joined by LF with NO trailing newline. Write this combined text into result-${name}.md inside Memory, result-${name}.txt inside Repository, and result.txt inside the injected outputs directory. Run git add -N result-${name}.txt and save git diff -- result-${name}.txt into repository.patch in the outputs directory. Do not publish to the remote. Reply RESOURCE_OK followed by the File bytes, a space, and this message's AWK_ATTEMPT value. Use the injected paths relative to your current working directory.`
        + (previous ? `\nAWK_PREVIOUS=${previous}\nBefore overwriting anything, verify that the existing Memory and Repository result files both still contain the same four resource lines followed by AWK_PREVIOUS. Stop with an error if either prior edit was lost during Worker replacement.` : '')
        + (native ? `\nAWK_TOOL_CWD=${trustedHost ? '.' : '/workspace'}${name === 'nested' ? '/repo/src' : ''}` : '')
        + (requireScopedExecution ? `\nAWK_PARENT_PID_NAMESPACE=${parentNamespace}\nVerify your actual /proc/self/ns/pid differs from AWK_PARENT_PID_NAMESPACE. Export that actual namespace and AWK_ATTEMPT joined by LF with no final newline as scope.txt in the outputs directory.` : '')
        + (contention ? `\nAWK_CONTEND=${contention}\nBefore editing shared paths, verify shared-a-identical.md and shared-z-conflict.md in Memory both equal seed.md exactly. Write seed.md bytes followed by LF and AWK_CONTEND (no final newline) to shared-a-identical.md. Write the five-line combined result text to shared-z-conflict.md. These are intentional same-base concurrent edits; do not retry by reading a newer durable base.` : ''),
      }] }],
    });
    const events = await allowManagedToolBoundaries({
      client, sessionId: session.id, taskReceiptId: receipt.data[0].id, betas,
      description: `TrustedHost ${runtime}/${name} resource I/O`,
      timeoutMs: 180_000, maxBoundaries: 30,
    });
    assert.ok(events.some((event) => event.type === 'agent.message'
      && JSON.stringify(event.content).includes(`RESOURCE_OK ${marker} ${attempt}`)),
      'R1 current receipt read exact File bytes, not a prior successful turn');
    const artifact = await waitForValue(async () => {
      const files = [];
      for await (const file of client.beta.files.list({ scope_id: session.id, betas })) files.push(file);
      for (const file of files.filter((entry) => entry.filename?.endsWith('result.txt'))) {
        if (await (await client.beta.files.download(file.id, { betas })).text() === expected) return file;
      }
      return undefined;
    }, Boolean, 'R1 output published through Files API', { timeoutMs: 20_000 });
    assert.equal(await (await client.beta.files.download(artifact.id, { betas })).text(), expected,
      'R1 downloaded artifact has exact bytes');
    let patch;
    let scopeProof;
    for await (const file of client.beta.files.list({ scope_id: session.id, betas })) {
      if (file.filename?.endsWith('repository.patch')
        && (await (await client.beta.files.download(file.id, { betas })).text()).includes(attempt)) patch = file;
      if (requireScopedExecution && file.filename?.endsWith('scope.txt')) {
        const proof = await (await client.beta.files.download(file.id, { betas })).text();
        if (proof.endsWith(`\n${attempt}`)) scopeProof = proof;
      }
    }
    if (requireScopedExecution) {
      assert.ok(scopeProof, 'S1 current resource receipt exports actual execution evidence');
      const [namespace, receipt] = scopeProof.split('\n');
      assert.match(namespace, /^pid:\[\d+\]$/u);
      assert.notEqual(namespace, parentNamespace, 'S2 resource I/O ran inside the PID boundary');
      assert.equal(receipt, attempt, 'S3 a prior namespace artifact cannot satisfy this receipt');
    }
    assert.ok(patch, 'R1 Repository changes exported through the same Artifact authority');
    const diff = await (await client.beta.files.download(patch.id, { betas })).text();
    assert.ok(diff.includes(`+++ b/result-${name}.txt`) && diff.includes(`+${marker}`),
      'R1 exact Repository edit appears in the downloaded Git diff');
    const verification = fs.mkdtempSync(path.join(temp, `patch-${name}-`));
    git(['clone', '-q', remote, verification]);
    execFileSync('git', ['apply', '--check', '-'], { cwd: verification, input: diff });
    execFileSync('git', ['apply', '-'], { cwd: verification, input: diff });
    assert.equal(fs.readFileSync(path.join(verification, `result-${name}.txt`), 'utf8'), expected,
      'R4 independently applying the patch reproduces all resource bytes and exact HEAD');
  };
  await Promise.all(cases.map(execute));
  // R3 restart keeps the SAME live Session IDs, bindings and physical projection;
  // run a new receipt through the recovered Session before releasing its mounts.
  // Stop semantics are platform-dependent; this proves process replacement,
  // not graceful shutdown on every OS or in-flight crash fencing.
  await restart();
  await Promise.all(cases.map(execute));
  // R5 dirty live Session + abrupt Worker death => same Session can
  // recover the surviving tree. The next mission MUST read the previous edits
  // before writing its new attempt marker; re-cloning would fail this oracle.
  await restart({ crash: true });
  contention = randomUUID();
  await Promise.all(cases.map(execute));
  // R7 terminal truth belongs to the Session repository, not DELETE returning
  // 200 or the winning Memory head. Capture each exact physical output locator
  // before release. A hidden Session compacts to its original tombstone only
  // after verified cleanup completes (admits_tombstone is checked in the SAME
  // mutation transaction); require that terminal receipt and actual removal.
  const aggregate = (id) => {
    const row = sqliteScalar(path.join(temp, 'data', 'sessions.db'),
      'SELECT aggregate_json FROM managed_session WHERE session_id = ?', id);
    assert.equal(typeof row, 'string', 'R7 original Session row remains the authority');
    const envelope = JSON.parse(row);
    assert.equal(envelope.format, 'awaken.session.v1');
    return envelope.aggregate;
  };
  const outputs = cases.map(({ session }) => {
    const handle = JSON.parse(aggregate(session.id).environment.binding);
    const logical = handle.payload.previous.outputs_path;
    assert.equal(typeof logical, 'string');
    // The handle's output path is LOGICAL, not a host path. Resolve the one
    // retained provider marker by physical incarnation; do not duplicate the
    // provider's portable sandbox-id hashing algorithm in this oracle.
    const suffix = '.awaken-realization.json';
    // The registered Worker may place Session roots below its own namespace,
    // not directly below the configured base. Search ONLY this test's owned
    // temporary tree, without following directory symlinks.
    const pending = [temp];
    const markers = [];
    while (pending.length) {
      const directory = pending.pop();
      for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
        const location = path.join(directory, entry.name);
        if (entry.isDirectory()) pending.push(location);
        else if (entry.isFile() && entry.name.startsWith('.') && entry.name.endsWith(suffix)
          && JSON.parse(fs.readFileSync(location, 'utf8')).physical_incarnation
            === handle.payload.physical_incarnation) markers.push(location);
      }
    }
    assert.equal(markers.length, 1, 'R7 one exact physical-generation marker');
    if (expectedPhysicalRoot) {
      // W5 separate Worker installation => retained physical evidence must be
      // below THAT root, never a successful embedded Worker projection. This
      // path is an independent oracle; it is never sent to the Agent fixture.
      const relative = path.relative(expectedPhysicalRoot, markers[0]);
      assert.ok(relative !== '' && relative !== '..' && !relative.startsWith('..' + path.sep)
        && !path.isAbsolute(relative), 'W5 resource realization belongs to standalone Worker');
    }
    const leaf = path.basename(markers[0]).slice(1, -suffix.length);
    const output = path.join(path.dirname(markers[0]), leaf, logical.replace(/^\/+/, ''));
    assert.ok(fs.existsSync(output), `R7 exact physical projection exists before cleanup: ${output}`);
    return output;
  });
  await Promise.all(cases.map(({ session }) => client.beta.sessions.delete(session.id, { betas })));
  const content = () => client.get(`/v1/memory_stores/${memory.id}/memories?view=full`, { headers: memoryHeaders });
  const complete = (page) => cases.every(({ name, expected }) =>
    page.data.some((entry) => entry.path === `/result-${name}.md` && entry.content === expected));
  await waitForValue(content, complete, 'R2 both concurrent Memory writes persisted', { timeoutMs: 20_000 });
  const shared = await waitForValue(content, (page) =>
    page.data.some((entry) => entry.path === '/shared-a-identical.md'
      && entry.content === `${memoryBytes}\n${contention}`)
    && page.data.some((entry) => entry.path === '/shared-z-conflict.md'
      && cases.some(({ expected }) => entry.content === expected)),
  'R6 mounted same-path writes converge or preserve one CAS winner', { timeoutMs: 20_000 });
  const assertSharedTruth = async () => {
    const page = await content();
    const versions = [];
    for await (const version of client.beta.memoryStores.memoryVersions.list(memory.id, {
      betas: ['agent-memory-2026-07-22'],
    })) versions.push(version);
    for (const initial of sharedHeads) {
      assert.deepEqual(page.data.find((entry) => entry.id === initial.id),
        shared.data.find((entry) => entry.id === initial.id), 'R6 exact winning head survives retry');
      assert.equal(versions.filter((version) => version.memory_id === initial.id).length, 2,
        'R6 exactly one initial version plus one winning update');
    }
  };
  await assertSharedTruth();
  // R8 same-base different proposal => preserve losing content+original base
  // in Memory, NOT Files and NOT a new winning version. The candidate survives
  // a Worker restart after the physical Session projections are reclaimed.
  const winner = shared.data.find((entry) => entry.path === '/shared-z-conflict.md');
  const loser = cases.find(({ expected }) => expected !== winner.content);
  assert.ok(loser);
  const proposals = () => client.get(
    `/v1/awaken/memory-stores/${memory.id}/copy-conflicts?view=full`, { headers: memoryHeaders });
  const retained = await waitForValue(proposals,
    (page) => page.data.length === 1 && page.data[0].content === loser.expected,
    'R8 exact losing proposal persisted', { timeoutMs: 20_000 });
  const original = sharedHeads.find((entry) => entry.path === '/shared-z-conflict.md');
  assert.equal(retained.data[0].path, original.path);
  assert.equal(retained.data[0].base.id, original.id);
  assert.equal(retained.data[0].base.content_sha256, original.content_sha256);
  const retired = () => cases.every(({ session }) => Number(sqliteScalar(
    path.join(temp, 'data', 'sessions.db'),
    `SELECT COUNT(*) FROM managed_session_tombstone t
     WHERE t.session_id = ? AND t.deleted_revision > 0
     AND NOT EXISTS (SELECT 1 FROM managed_session s WHERE s.session_id = t.session_id)`,
    session.id,
  )) === 1);
  await waitForValue(async () => retired(), Boolean,
    'R7 both verified cleanups committed their canonical delete tombstones', { timeoutMs: 30_000 });
  for (const output of outputs) assert.ok(!fs.existsSync(output), 'R7 exact physical output projection removed');
  assert.equal(git(['rev-parse', 'HEAD'], remote), remoteHead, 'R2 no implicit Git publication');
  assert.equal(git(['ls-tree', '-r', '--name-only', 'HEAD'], remote), 'README.md\nsrc/.keep');
  await restart();
  await waitForValue(content, complete, 'R3 actual Worker restart retains both writes', { timeoutMs: 20_000 });
  await assertSharedTruth();
  assert.deepEqual((await proposals()).data, retained.data, 'R8 retained candidate survives restart');
  assert.ok(retired(), 'R7 completed cleanup tombstones survive restart without resurrection');
  console.log(`AGENT RESOURCE PASS: ${runtime}, ${native ? 'default/nested Native tool cwd' : 'two ACP cwd views'}, File/Memory/Repository, same-path convergence/conflict, restart`);
}
