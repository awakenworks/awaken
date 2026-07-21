// Cause/effect E2E for the shared resource persistence axis. Two awaken
// processes use different local directories but one Postgres resource backend;
// File bytes, Memory content/history, Skill bundles, and lifecycle state must all
// survive. Authorization/IAM is deliberately absent from the resource backend.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, execSync, spawn, type ChildProcess } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38436);
const WORKSPACE = `resource-pg-${process.pid}`;
const OTHER_WORKSPACE = `resource-pg-other-${process.pid}`;
const AGENT = `resource-pg-agent-${process.pid}`;
const MODEL = `resource-pg-model-${process.pid}`;
const OWN_BUILD_TARGET = process.env.CARGO_TARGET_DIR === undefined;
const BUILD_TARGET = process.env.CARGO_TARGET_DIR ??
  path.join(os.tmpdir(), `awaken-resource-plane-e2e-target-${process.pid}`);
const sleep = (milliseconds: number) => new Promise((resolve) => setTimeout(resolve, milliseconds));

function docker(...args: string[]): string {
  return execFileSync('docker', args, { cwd: ROOT, encoding: 'utf8' }).trim();
}

async function postgres(): Promise<{ container: string; url: string; owned: boolean }> {
  const inheritedUrl = process.env.AWAKEN_DATABASE_URL;
  const inheritedContainer = process.env.AWAKEN_E2E_POSTGRES_CONTAINER;
  if (inheritedUrl && inheritedContainer) {
    return { container: inheritedContainer, url: inheritedUrl, owned: false };
  }
  const container = `awaken-resource-plane-pg-${process.pid}`;
  docker(
    'run', '-d', '--name', container,
    '-e', 'POSTGRES_PASSWORD=test',
    '-e', 'POSTGRES_DB=awaken',
    '-p', '127.0.0.1::5432',
    '--health-cmd=pg_isready -U postgres -d awaken',
    '--health-interval=1s', '--health-timeout=2s', '--health-retries=30',
    'postgres:16-alpine',
  );
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const health = docker('inspect', '--format', '{{.State.Health.Status}}', container);
    if (health === 'healthy') {
      const mapping = docker('port', container, '5432/tcp').split('\n')[0];
      const port = mapping.slice(mapping.lastIndexOf(':') + 1);
      return {
        container,
        url: `postgres://postgres:test@127.0.0.1:${port}/awaken`,
        owned: true,
      };
    }
    await sleep(250);
  }
  throw new Error('timed out waiting for disposable Postgres');
}

function binary(): string {
  const output = execSync('cargo build --quiet --message-format=json -p awaken-cli --bin awaken', {
    cwd: ROOT,
    env: { ...process.env, CARGO_TARGET_DIR: BUILD_TARGET },
    maxBuffer: 64 * 1024 * 1024,
  }).toString();
  for (const line of output.split('\n')) {
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken') return message.executable;
    } catch { /* cargo diagnostic */ }
  }
  throw new Error('awaken binary was not produced');
}

function start(bin: string, directory: string, databaseUrl: string): ChildProcess {
  const inherited = { ...process.env };
  delete inherited.AWAKEN_DATABASE_URL;
  delete inherited.AWAKEN_RUNTIME_DISPATCH_DATABASE_URL;
  delete inherited.AWAKEN_STORE;
  delete inherited.AWAKEN_DISPATCH_BACKEND;
  return spawn(bin, {
    env: {
      ...inherited,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_LOCAL_WORKSPACE_ID: WORKSPACE,
      AWAKEN_STORAGE_DIR: directory,
      AWAKEN_DEPLOYMENT_DATA_DIR: directory,
      AWAKEN_CONTROL_SEAL_KEY: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
      AWAKEN_RESOURCE_DATABASE_URL: databaseUrl,
      // MemoryStore definitions and Agent resource bindings are configuration
      // plane facts. They are shared separately from resource content and IAM.
      AWAKEN_ADMIN_DB: databaseUrl,
    },
    stdio: ['ignore', 'ignore', 'inherit'],
  });
}

async function ready(): Promise<void> {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const connected = await new Promise<boolean>((resolve) => {
      const socket = net.createConnection({ host: '127.0.0.1', port: PORT });
      socket.once('connect', () => { socket.destroy(); resolve(true); });
      socket.once('error', () => { socket.destroy(); resolve(false); });
    });
    if (connected) return;
    await sleep(100);
  }
  throw new Error('awaken did not become ready');
}

async function stop(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null) return;
  child.kill('SIGINT');
  await new Promise((resolve) => child.once('exit', resolve));
}

const scoped = (workspace: string, suffix: string) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${workspace}/${suffix}`;

async function json(method: string, url: string, body?: unknown) {
  const response = await fetch(url, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function upload(content: string, workspace = WORKSPACE): Promise<string> {
  const form = new FormData();
  form.append('purpose', 'agent');
  form.append('file', new Blob([content]), 'shared.txt');
  const response = await fetch(scoped(workspace, 'files'), { method: 'POST', body: form });
  assert.equal(response.status, 200);
  return (await response.json()).id;
}

async function uploadSkillVersion(route: string, marker: string, binary?: Uint8Array) {
  const form = new FormData();
  form.append(
    'file',
    new Blob([
      `---\nname: shared-skill-${process.pid}\ndescription: shared resource test\n---\n${marker}`,
    ], { type: 'text/markdown' }),
    'SKILL.md',
  );
  if (binary !== undefined) {
    form.append(
      'file',
      new Blob([binary], { type: 'application/octet-stream' }),
      'assets/data.bin',
    );
  }
  const response = await fetch(scoped(WORKSPACE, route), { method: 'POST', body: form });
  const body = await response.json().catch(() => ({}));
  assert.equal(response.status, 200, `${route}: ${JSON.stringify(body)}`);
  return body;
}

function assertNoLocalResourceTruth(directory: string): void {
  for (const relative of ['files.db', 'memory_fs.db', 'resource-lifecycle.db', 'skills']) {
    assert.equal(
      fs.existsSync(path.join(directory, relative)),
      false,
      `${relative} must not become a node-local second resource truth`,
    );
  }
}

function psql(container: string, sql: string): string {
  return docker('exec', container, 'psql', '-U', 'postgres', '-d', 'awaken', '-At', '-c', sql);
}

function seedRepository(root: string): string {
  const work = path.join(root, 'repository-work');
  const remote = path.join(root, 'repository.git');
  fs.mkdirSync(work, { recursive: true });
  execFileSync('git', ['init', '-q', '-b', 'main'], { cwd: work });
  execFileSync('git', ['config', 'user.email', 'resource-e2e@example.invalid'], { cwd: work });
  execFileSync('git', ['config', 'user.name', 'resource-e2e'], { cwd: work });
  fs.writeFileSync(path.join(work, 'README.md'), 'governed repository');
  execFileSync('git', ['add', 'README.md'], { cwd: work });
  execFileSync('git', ['commit', '-q', '-m', 'seed'], { cwd: work });
  execFileSync('git', ['clone', '-q', '--bare', work, remote]);
  return remote;
}

async function publishAgent(): Promise<void> {
  assert.equal((await json('PUT', scoped(WORKSPACE, 'config/providers/resource-e2e'), {
    id: 'resource-e2e', slug: 'resource-e2e', display_name: 'Resource E2E', version: 1,
  })).status, 200);
  assert.equal((await json('PUT', scoped(WORKSPACE, 'config/endpoints/resource-e2e'), {
    id: 'resource-e2e', provider_id: 'resource-e2e', dialect: 'anthropic_messages',
    base_url: 'http://127.0.0.1:1/v1/', timeout_secs: 10, display_name: 'unused', version: 1,
  })).status, 200);
  assert.equal((await json('POST', scoped(WORKSPACE, 'config/offerings'), {
    model_id: MODEL, provider_id: 'resource-e2e', protocol_endpoint_id: 'resource-e2e',
    dialect: 'anthropic_messages', upstream_model: null,
  })).status, 200);
  assert.equal((await json('PUT', scoped(WORKSPACE, `config/model-attributes/${MODEL}`), {
    context_window: 4096, max_output_tokens: 1024,
  })).status, 200);
  assert.equal((await json('POST', scoped(WORKSPACE, 'config/credentials'), {
    workspace_id: WORKSPACE, kind: 'vault', provider_id: 'resource-e2e',
    env_key: null, secret: 'resource-e2e-model-key', // awaken-allow: secret
  })).status, 201);
  assert.equal((await json('PUT', scoped(WORKSPACE, `config/agents/${AGENT}`), {
    name: AGENT, model: { id: MODEL }, system: 'Resource lifecycle test.',
    max_steps: 2, plugins: [], plugin_config: {},
  })).status, 200);
  assert.equal((await json('PUT', scoped(WORKSPACE, `config/agents/${AGENT}/resources`), {
    agent_id: AGENT, revision: 1, inputs: [],
  })).status, 200);
  const published = await json('POST', scoped(WORKSPACE, `config/agents/${AGENT}/publish`));
  assert.equal(published.status, 200, JSON.stringify(published.body));
}

async function main(): Promise<void> {
  const pg = await postgres();
  const firstDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-resource-pg-a-'));
  const secondDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-resource-pg-b-'));
  const bin = binary();
  let server = start(bin, firstDirectory, pg.url);
  try {
    await ready();
    const fileId = await upload('shared postgres file bytes');
    assert.equal(
      await upload('shared postgres file bytes'),
      fileId,
      'equal immutable bytes are idempotent inside a Workspace',
    );
    const fileMetadata = await json('GET', scoped(WORKSPACE, `files/${fileId}`));
    assert.equal(fileMetadata.status, 200);
    assert.equal(fileMetadata.body.size_bytes, 'shared postgres file bytes'.length);

    const memoryStore = await json('POST', scoped(WORKSPACE, 'memory_stores'), {
      name: 'shared-memory',
      description: 'before restart',
      metadata: { phase: 'created', remove_me: 'yes' },
    });
    assert.equal(memoryStore.status, 200);
    const memoryId = memoryStore.body.id;
    const memoryStores = await json('GET', scoped(WORKSPACE, 'memory_stores'));
    assert.equal(memoryStores.status, 200);
    assert.ok(memoryStores.body.data.some((item: { id: string }) => item.id === memoryId));
    const initialMemoryConfig = await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/config`));
    assert.equal(initialMemoryConfig.status, 200);
    assert.equal(initialMemoryConfig.body.version, 1);
    const publishedMemoryConfig = await json(
      'POST',
      scoped(WORKSPACE, `memory_stores/${memoryId}/config`),
      {
        expected_config_version: 1,
        recall_policy: { enabled: true, max_results: 19 },
        extraction_policy: { enabled: false },
        retention_policy: { retention_days: 2 },
      },
    );
    assert.equal(publishedMemoryConfig.status, 200);
    assert.equal(publishedMemoryConfig.body.version, 2);
    assert.equal(
      (await json('POST', scoped(WORKSPACE, `memory_stores/${memoryId}/config`), {
        expected_config_version: 1,
        recall_policy: { enabled: false, max_results: 1 },
      })).status,
      409,
    );
    const patchedStore = await json('POST', scoped(WORKSPACE, `memory_stores/${memoryId}`), {
      description: 'persisted catalog update',
      metadata: { phase: 'updated', remove_me: null },
    });
    assert.equal(patchedStore.status, 200);
    assert.deepEqual(patchedStore.body.metadata, { phase: 'updated' });
    assert.equal(
      (await json('POST', scoped(WORKSPACE, `memory_stores/${memoryId}/memories`), {
        content: 'missing path',
      })).status,
      400,
    );
    const memory = await json('POST', scoped(WORKSPACE, `memory_stores/${memoryId}/memories`), {
      path: '/fact.md',
      content: 'shared postgres memory',
    });
    assert.equal(memory.status, 200);
    const memoryEntryId = memory.body.id;
    const initialSha = memory.body.content_sha256;
    assert.equal(
      (await json('POST', scoped(WORKSPACE, `memory_stores/${memoryId}/memories`), {
        path: '/fact.md',
        content: 'conflicting create',
      })).status,
      409,
    );
    assert.equal(
      (await json('POST', scoped(WORKSPACE, `memory_stores/${memoryId}/memories/${memoryEntryId}`), {
        content: 'must not win',
        precondition: { content_sha256: 'stale' },
      })).status,
      409,
    );
    const updatedMemory = await json(
      'POST',
      scoped(WORKSPACE, `memory_stores/${memoryId}/memories/${memoryEntryId}`),
      {
        path: '/renamed-fact.md',
        content: 'shared postgres memory v2',
        precondition: { content_sha256: initialSha },
      },
    );
    assert.equal(updatedMemory.status, 200);
    assert.equal(updatedMemory.body.path, '/renamed-fact.md');
    const basicMemories = await json(
      'GET',
      `${scoped(WORKSPACE, `memory_stores/${memoryId}/memories`)}?path_prefix=/&view=basic`,
    );
    assert.equal(basicMemories.status, 200);
    assert.equal(basicMemories.body.data[0].path, '/renamed-fact.md');
    assert.equal(basicMemories.body.data[0].content, null);
    assert.equal(
      (await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/memories/${memoryEntryId}`))).status,
      200,
    );

    const skill = await json('POST', scoped(WORKSPACE, 'skills'), {
      id: `shared-skill-${process.pid}`,
      content: `---\nname: shared-skill-${process.pid}\ndescription: shared resource test\n---\nUse safely.`,
    });
    assert.equal(skill.status, 200);
    const skillId = skill.body.id;
    assert.equal(
      (await json('POST', scoped(WORKSPACE, 'skills'), {
        id: skillId,
        content: `---\nname: ${skillId}\ndescription: duplicate\n---\nduplicate`,
      })).status,
      409,
    );
    const binaryFixture = Uint8Array.from([0, 159, 146, 150, 255, 13, 0, 10]);
    const skillV2 = await uploadSkillVersion(
      `skills/${skillId}/versions`,
      'shared postgres skill v2',
      binaryFixture,
    );
    assert.equal(skillV2.version, '2');
    const skills = await json('GET', scoped(WORKSPACE, 'skills'));
    assert.ok(skills.body.data.some((item: { id: string }) => item.id === skillId));
    await stop(server);
    assertNoLocalResourceTruth(firstDirectory);

    server = start(bin, secondDirectory, pg.url);
    await ready();
    const fileResponse = await fetch(scoped(WORKSPACE, `files/${fileId}/content`));
    assert.equal(fileResponse.status, 200);
    assert.equal(await fileResponse.text(), 'shared postgres file bytes');
    const memories = await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/memories`));
    assert.equal(memories.status, 200);
    assert.equal(memories.body.data[0].content, 'shared postgres memory v2');
    assert.equal(memories.body.data[0].path, '/renamed-fact.md');
    const restoredStore = await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}`));
    assert.equal(restoredStore.body.description, 'persisted catalog update');
    assert.deepEqual(restoredStore.body.metadata, { phase: 'updated' });
    const restoredMemoryConfig = await json(
      'GET',
      scoped(WORKSPACE, `memory_stores/${memoryId}/config`),
    );
    assert.equal(restoredMemoryConfig.body.version, 2);
    assert.equal(restoredMemoryConfig.body.recall_policy.max_results, 19);
    assert.equal(
      (await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/config_versions/1`))).body.version,
      1,
    );
    const versions = await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/memory_versions`));
    assert.equal(versions.status, 200);
    assert.ok(versions.body.data.length >= 2);
    const firstVersionId = versions.body.data[0].id;
    assert.equal(
      (await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/memory_versions/${firstVersionId}`))).status,
      200,
    );
    const redacted = await json(
      'POST',
      scoped(WORKSPACE, `memory_stores/${memoryId}/memory_versions/${firstVersionId}/redact`),
    );
    assert.equal(redacted.status, 200);
    assert.equal(redacted.body.content, null);
    assert.notEqual(redacted.body.redacted_at, null);
    assert.equal((await json('GET', scoped(WORKSPACE, `skills/${skillId}`))).status, 200);
    const skillVersions = await json('GET', scoped(WORKSPACE, `skills/${skillId}/versions`));
    assert.equal(skillVersions.status, 200);
    assert.equal(skillVersions.body.data.length, 2);
    const latestSkill = await json('GET', scoped(WORKSPACE, `skills/${skillId}/versions/latest`));
    assert.equal(latestSkill.status, 200);
    assert.equal(latestSkill.body.version, '2');
    const skillContent = await fetch(scoped(WORKSPACE, `skills/${skillId}/versions/2/content`));
    assert.equal(skillContent.status, 200);
    assert.match(await skillContent.text(), /shared postgres skill v2/u);
    const skillBinary = await fetch(
      scoped(WORKSPACE, `skills/${skillId}/versions/2/files/assets/data.bin`),
    );
    assert.equal(skillBinary.status, 200);
    assert.deepEqual(new Uint8Array(await skillBinary.arrayBuffer()), binaryFixture);

    // Workspace routing/ownership is intrinsic resource state. It fails closed
    // without putting a principal, role, token, or policy inside any content port.
    assert.equal((await fetch(scoped(OTHER_WORKSPACE, `files/${fileId}/content`))).status, 404);
    assert.equal((await json('GET', scoped(OTHER_WORKSPACE, `memory_stores/${memoryId}`))).status, 404);
    assert.equal((await json('GET', scoped(OTHER_WORKSPACE, `skills/${skillId}`))).status, 404);

    // Drive the production Managed Session edge so Repository configuration uses
    // this same PostgreSQL Resource Catalog rather than a scenario-host registry.
    await publishAgent();
    const repository = seedRepository(secondDirectory);
    const session = await json('POST', scoped(WORKSPACE, 'sessions'), {
      agent: AGENT, environment_id: 'env_local',
      resources: [{
        type: 'github_repository',
        url: repository,
        authorization_token: 'repository-create-token', // awaken-allow: secret
        initial_branch: 'main',
        mount_path: '/workspace/create-time-repository',
      }],
    });
    assert.equal(session.status, 200, JSON.stringify(session.body));
    assert.equal(session.body.resources[0].type, 'github_repository');
    const repositoryResource = await json(
      'POST',
      scoped(WORKSPACE, `sessions/${session.body.id}/resources`),
      { type: 'github_repository', url: repository, mount_path: '/workspace/repository' },
    );
    assert.equal(repositoryResource.status, 200, JSON.stringify(repositoryResource.body));
    const updatedRepository = await json(
      'POST',
      scoped(WORKSPACE, `sessions/${session.body.id}/resources/${repositoryResource.body.id}`),
      {
        mount_path: '/workspace/repository-updated',
        authorization_token: 'repository-rotated-token', // awaken-allow: secret
      },
    );
    assert.equal(updatedRepository.status, 200, JSON.stringify(updatedRepository.body));
    assert.equal(updatedRepository.body.mount_path, '/workspace/repository-updated');
    const retiredRepository = await json(
      'DELETE',
      scoped(WORKSPACE, `sessions/${session.body.id}/resources/${repositoryResource.body.id}`),
    );
    assert.equal(retiredRepository.status, 200, JSON.stringify(retiredRepository.body));
    assert.equal(retiredRepository.body.type, 'session_resource_deleted');

    // The immutable File blob may have more than one Workspace ownership edge.
    // Removing one edge must deny that Workspace immediately without deleting
    // bytes still owned by another Workspace.
    assert.equal(await upload('shared postgres file bytes', OTHER_WORKSPACE), fileId);
    assert.equal((await fetch(scoped(OTHER_WORKSPACE, `files/${fileId}/content`))).status, 200);
    assert.equal((await json('DELETE', scoped(WORKSPACE, `files/${fileId}`))).status, 200);
    assert.equal((await fetch(scoped(WORKSPACE, `files/${fileId}/content`))).status, 404);
    assert.equal((await fetch(scoped(OTHER_WORKSPACE, `files/${fileId}/content`))).status, 200);
    assert.equal((await json('DELETE', scoped(OTHER_WORKSPACE, `files/${fileId}`))).status, 200);

    assert.equal(
      (await json('DELETE', scoped(WORKSPACE, `memory_stores/${memoryId}/memories/${memoryEntryId}`))).status,
      200,
    );
    assert.equal(
      (await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/memories/${memoryEntryId}`))).status,
      404,
    );
    assert.equal((await json('DELETE', scoped(WORKSPACE, `memory_stores/${memoryId}`))).status, 200);
    const deletedStore = await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}`));
    assert.equal(deletedStore.status, 200);
    assert.notEqual(deletedStore.body.archived_at, null);
    assert.equal(
      (await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/memories`))).status,
      404,
    );
    assert.equal((await json('DELETE', scoped(WORKSPACE, `skills/${skillId}/versions/1`))).status, 200);
    assert.equal((await json('GET', scoped(WORKSPACE, `skills/${skillId}/versions/1`))).status, 404);
    assert.equal((await json('DELETE', scoped(WORKSPACE, `skills/${skillId}`))).status, 200);
    assert.equal((await json('GET', scoped(WORKSPACE, `skills/${skillId}`))).status, 404);
    assertNoLocalResourceTruth(secondDirectory);

    const tables = psql(
      pg.container,
      "SELECT count(*) FROM information_schema.tables WHERE table_schema='public' AND " +
        "(table_name LIKE 'file_store_%' OR table_name LIKE 'memory_store_%' OR " +
        "table_name LIKE 'skill_store_%' OR table_name LIKE 'resource_lifecycle_%')",
    );
    assert.ok(Number(tables) >= 10, `all resource migration scopes exist, got ${tables}`);
    const authColumns = psql(
      pg.container,
      "SELECT table_name || ':' || column_name FROM information_schema.columns " +
        "WHERE table_schema='public' AND (table_name LIKE 'file_store_%' OR " +
        "table_name LIKE 'memory_store_%' OR table_name LIKE 'skill_store_%' OR " +
        "table_name LIKE 'resource_lifecycle_%') AND lower(column_name) ~ " +
        "'(principal|api_key|token|role|policy|decision|credential|org|project|work_unit)'",
    );
    assert.equal(authColumns, '', `resource persistence leaked authorization columns: ${authColumns}`);
    console.log('E2E PASS: complete Postgres resource plane is shared, scoped, and IAM-independent.');
  } finally {
    await stop(server).catch(() => {});
    fs.rmSync(firstDirectory, { recursive: true, force: true });
    fs.rmSync(secondDirectory, { recursive: true, force: true });
    if (OWN_BUILD_TARGET) fs.rmSync(BUILD_TARGET, { recursive: true, force: true });
    if (pg.owned) {
      try { docker('rm', '-f', pg.container); } catch { /* best effort */ }
    }
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
