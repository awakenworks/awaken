import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import { toFile } from '@anthropic-ai/sdk';
import { allowManagedToolBoundaries, waitForValue } from '../harness.mjs';

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

export async function verifyTrustedHostResources({ client, request, temp, runtime, restart }) {
  // Cause/effect graph:
  // C1 API binds trusted_host version 1; C2 cwd={default,repo/src};
  // C3 File+Memory+Repository; C4 two Sessions share a MemoryStore and host
  // identity; C5 release and Worker restart. E1 actual file I/O follows rendered
  // paths; E2 concurrent distinct writes persist; E3 remote Git is unchanged;
  // E4 restart retains both writes. Rules R1=C1+C2+C3=>E1; R2=R1+C4=>E2+E3;
  // R3=R2+C5=>E4. Cloud+TrustedHost and invalid policy => reject before Session.
  // The live adapter and deterministic peer run this SAME mission and oracle.
  const betas = ['managed-agents-2026-04-01'];
  const memoryHeaders = { 'anthropic-beta': 'agent-memory-2026-07-22' };
  const api = async (method, route, body, expected = 200) => {
    const result = await request(method, route, body);
    assert.equal(result.response.status, expected, `${route}: ${JSON.stringify(result.value)}`);
    return result.value;
  };
  await api('POST', '/v1/awaken/sandbox-execution-policies', {
    id: 'resource-host', config: { isolation: 'workdir', host_access: 'trusted_host' },
  }, 201);
  const environment = await api('POST', '/v1/awaken/environments', {
    command_id: 'resource-host-create', name: 'Resource host',
    config: { type: 'self_hosted' }, sandbox_policy: { policy_id: 'resource-host', version: 1 },
  }, 201);
  assert.deepEqual(environment.sandbox_policy, { policy_id: 'resource-host', version: 1 });
  await api('POST', '/v1/awaken/environments', {
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
  fs.writeFileSync(path.join(seed, 'README.md'), 'REPOSITORY_SEED');
  fs.writeFileSync(path.join(seed, 'src', '.keep'), '');
  git(['add', '.']);
  git(['commit', '-qm', 'seed']);
  git(['clone', '-q', '--bare', seed, remote]);
  const remoteHead = git(['rev-parse', 'HEAD'], remote);
  const memory = await client.post('/v1/memory_stores', {
    headers: memoryHeaders, body: { name: 'shared host memory' },
  });
  await client.post(`/v1/memory_stores/${memory.id}/memories`, {
    headers: memoryHeaders, body: { path: '/seed.md', content: 'MEMORY_SEED' },
  });
  const cases = [];
  for (const [name, cwd] of [['default', undefined], ['nested', 'repo/src']]) {
    const agent = `resource-${runtime}-${name}`;
    await api('PUT', `/v1/config/agents/${agent}`, {
      name: agent, model: {
        mode: 'backend_default', backend_ref: `acp:${runtime}`,
        ...(cwd ? { configuration: { working_directory: cwd } } : {}),
      },
      system: 'Follow the resource mission exactly. Read resource instructions before file operations.',
      tools: [],
    });
    await api('POST', `/v1/config/agents/${agent}/publish`);
    const marker = `RESOURCE_BYTES_${runtime}_${name}_${Date.now()}`;
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
    cases.push({ session, marker, name });
  }
  await Promise.all(cases.map(async ({ session, marker, name }) => {
    const receipt = await client.beta.sessions.events.send(session.id, {
      betas, events: [{ type: 'user.message', content: [{ type: 'text', text:
        `AWK_RESOURCE_MISSION\nAWK_RESULT=result-${name}\nRead the uploaded File using its mounted path. Its exact bytes are a secret marker not supplied in this user message. Read README.md in the mounted Repository and seed.md in Memory. Copy the File bytes, with no added newline, into result-${name}.md inside Memory, result-${name}.txt inside Repository, and result.txt inside the injected outputs directory. Use git rev-parse HEAD in the Repository. Run git add -N result-${name}.txt and save git diff -- result-${name}.txt into repository.patch in the outputs directory. Do not publish to the remote. Reply RESOURCE_OK followed by the File bytes. Use the injected paths relative to your current working directory.`,
      }] }],
    });
    const events = await allowManagedToolBoundaries({
      client, sessionId: session.id, taskReceiptId: receipt.data[0].id, betas,
      description: `TrustedHost ${runtime}/${name} resource I/O`,
      timeoutMs: 180_000, maxBoundaries: 30,
    });
    assert.ok(events.some((event) => event.type === 'agent.message'
      && JSON.stringify(event.content).includes(`RESOURCE_OK ${marker}`)), 'R1 read exact File bytes');
    const artifact = await waitForValue(async () => {
      const files = [];
      for await (const file of client.beta.files.list({ scope_id: session.id, betas })) files.push(file);
      return files.find((file) => file.filename?.endsWith('result.txt'));
    }, Boolean, 'R1 output published through Files API', { timeoutMs: 20_000 });
    assert.equal(await (await client.beta.files.download(artifact.id, { betas })).text(), marker,
      'R1 downloaded artifact has exact bytes');
    let patch;
    for await (const file of client.beta.files.list({ scope_id: session.id, betas })) {
      if (file.filename?.endsWith('repository.patch')) patch = file;
    }
    assert.ok(patch, 'R1 Repository changes exported through the same Artifact authority');
    const diff = await (await client.beta.files.download(patch.id, { betas })).text();
    assert.ok(diff.includes(`+++ b/result-${name}.txt`) && diff.includes(`+${marker}`),
      'R1 exact Repository edit appears in the downloaded Git diff');
    await client.beta.sessions.delete(session.id, { betas });
  }));
  const content = () => client.get(`/v1/memory_stores/${memory.id}/memories?view=full`, { headers: memoryHeaders });
  const complete = (page) => cases.every(({ name, marker }) =>
    page.data.some((entry) => entry.path === `/result-${name}.md` && entry.content === marker));
  await waitForValue(content, complete, 'R2 both concurrent Memory writes persisted', { timeoutMs: 20_000 });
  assert.equal(git(['rev-parse', 'HEAD'], remote), remoteHead, 'R2 no implicit Git publication');
  assert.equal(git(['ls-tree', '-r', '--name-only', 'HEAD'], remote), 'README.md\nsrc/.keep');
  await restart();
  await waitForValue(content, complete, 'R3 actual Worker restart retains both writes', { timeoutMs: 20_000 });
  console.log(`TRUSTED HOST RESOURCE PASS: ${runtime}, two cwd views, File/Memory/Repository, concurrent writeback, restart`);
}
