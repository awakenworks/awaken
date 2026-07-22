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
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

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
      // Session application work (including durable extraction intents) is an
      // independent persistence axis; select it explicitly instead of deriving
      // it from the resource backend.
      AWAKEN_SESSIONS_DB: databaseUrl,
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
    headers: {
      'anthropic-beta': 'managed-agents-2026-04-01',
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
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

function sqlLiteral(value: string): string {
  return `'${value.replaceAll("'", "''")}'`;
}

function resourceCatalogRecord(
  container: string,
  kind: string,
  id: string,
): Record<string, any> {
  const output = psql(
    container,
    `SELECT data FROM admin_resource_catalog WHERE kind=${sqlLiteral(kind)} AND id=${sqlLiteral(id)}`,
  );
  assert.notEqual(output, '', `missing ${kind} Resource Catalog row ${id}`);
  return JSON.parse(output);
}

function writeResourceCatalogRecord(
  container: string,
  kind: string,
  id: string,
  record: Record<string, any>,
): void {
  psql(
    container,
    `UPDATE admin_resource_catalog SET data=${sqlLiteral(JSON.stringify(record))}::jsonb ` +
      `WHERE kind=${sqlLiteral(kind)} AND id=${sqlLiteral(id)}`,
  );
}

function seedLegacyMemoryIdentities(container: string, canonicalId: string): void {
  const rows = [
    {
      id: `legacy-pg-owned-${process.pid}`,
      workspace_id: WORKSPACE,
      name: 'Postgres legacy governed memory',
      description: 'owned v6 row',
      metadata: { source: 'admin-v6' },
      archived: false,
    },
    {
      id: `legacy-pg-unowned-${process.pid}`,
      name: 'Must remain quarantined',
      archived: false,
    },
    {
      id: canonicalId,
      workspace_id: WORKSPACE,
      name: 'Must not replace canonical Postgres history',
      archived: false,
    },
  ];
  psql(
    container,
    rows.map((row) =>
      `INSERT INTO admin_memory_store(id, data) VALUES (${sqlLiteral(row.id)}, ${sqlLiteral(JSON.stringify(row))}::jsonb);`,
    ).join(' '),
  );
}

function resourceIntent(container: string, resourceId: string): Record<string, any> | undefined {
  const output = psql(
    container,
    `SELECT data FROM resource_lifecycle_purge_intents ` +
      `WHERE data::jsonb->'target'->>'resource_id'=${sqlLiteral(resourceId)} ` +
      `ORDER BY requested_at_unix_ms DESC LIMIT 1`,
  );
  return output ? JSON.parse(output) : undefined;
}

function managedSessionResources(
  container: string,
  sessionId: string,
): Record<string, any> {
  const output = psql(
    container,
    `SELECT effective_inputs_json FROM managed_session WHERE session_id=${sqlLiteral(sessionId)}`,
  );
  assert.notEqual(output, '', `missing durable Session ${sessionId}`);
  return JSON.parse(output);
}

function writeManagedSessionResources(
  container: string,
  sessionId: string,
  resources: Record<string, any>,
): void {
  psql(
    container,
    `UPDATE managed_session SET effective_inputs_json=${sqlLiteral(JSON.stringify(resources))}::jsonb ` +
      `WHERE session_id=${sqlLiteral(sessionId)}`,
  );
}

function extractionIntent(
  container: string,
  sessionId: string,
): Record<string, any> | undefined {
  const output = psql(
    container,
    `SELECT data FROM managed_memory_extraction ` +
      `WHERE data::jsonb->>'session_id'=${sqlLiteral(sessionId)} ` +
      `ORDER BY created_at DESC LIMIT 1`,
  );
  return output ? JSON.parse(output) : undefined;
}

async function waitForCompletedExtraction(
  container: string,
  sessionId: string,
  timeoutMs = 30_000,
): Promise<Record<string, any>> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const intent = extractionIntent(container, sessionId);
    if (intent?.status === 'completed' && intent.receipt !== null) return intent;
    await sleep(250);
  }
  throw new Error(
    `Postgres extraction intent did not complete: ${JSON.stringify(extractionIntent(container, sessionId))}`,
  );
}

async function waitForResourceIntents(
  container: string,
  ids: string[],
  predicate: (intent: Record<string, any>) => boolean,
  timeoutMs = 30_000,
): Promise<Record<string, any>[]> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const rows = ids.map((id) => resourceIntent(container, id));
    if (rows.every((row) => row !== undefined && predicate(row))) {
      return rows as Record<string, any>[];
    }
    await sleep(250);
  }
  throw new Error(`Postgres resource intents did not converge: ${JSON.stringify(ids.map((id) => resourceIntent(container, id)))}`);
}

function installReclamationFaults(
  container: string,
  ids: { contended: string; late: string; release: string; physical: string },
): void {
  const suffix = String(process.pid);
  psql(container, `
    INSERT INTO resource_lifecycle_reclamation_fences(resource_kind, resource_id, intent_id)
      VALUES ('file', ${sqlLiteral(ids.contended)}, 'external-postgres-reclaimer');
    CREATE FUNCTION inject_late_reference_${suffix}() RETURNS trigger LANGUAGE plpgsql AS $$
    BEGIN
      IF NEW.resource_id = ${sqlLiteral(ids.late)} THEN
        INSERT INTO resource_lifecycle_references(
          workspace_id, resource_kind, resource_id, reference_kind, reference_id
        ) VALUES (${sqlLiteral(WORKSPACE)}, 'file', NEW.resource_id, 'session_binding', 'late-postgres-reference');
      END IF;
      RETURN NEW;
    END $$;
    CREATE TRIGGER inject_late_reference_${suffix}
      AFTER INSERT ON resource_lifecycle_reclamation_fences
      FOR EACH ROW EXECUTE FUNCTION inject_late_reference_${suffix}();
    CREATE FUNCTION reject_fence_release_${suffix}() RETURNS trigger LANGUAGE plpgsql AS $$
    BEGIN
      IF OLD.resource_id = ${sqlLiteral(ids.release)} THEN
        RAISE EXCEPTION 'injected postgres fence release failure';
      END IF;
      RETURN OLD;
    END $$;
    CREATE TRIGGER reject_fence_release_${suffix}
      BEFORE DELETE ON resource_lifecycle_reclamation_fences
      FOR EACH ROW EXECUTE FUNCTION reject_fence_release_${suffix}();
    CREATE FUNCTION reject_blob_delete_${suffix}() RETURNS trigger LANGUAGE plpgsql AS $$
    BEGIN
      IF OLD.id = ${sqlLiteral(ids.physical)} THEN
        RAISE EXCEPTION 'injected postgres blob delete failure';
      END IF;
      RETURN OLD;
    END $$;
    CREATE TRIGGER reject_blob_delete_${suffix}
      BEFORE DELETE ON file_store_blob
      FOR EACH ROW EXECUTE FUNCTION reject_blob_delete_${suffix}();
  `);
}

function removeReclamationFaults(
  container: string,
  ids: { contended: string; late: string },
): void {
  const suffix = String(process.pid);
  psql(container, `
    DROP TRIGGER inject_late_reference_${suffix} ON resource_lifecycle_reclamation_fences;
    DROP FUNCTION inject_late_reference_${suffix}();
    DROP TRIGGER reject_fence_release_${suffix} ON resource_lifecycle_reclamation_fences;
    DROP FUNCTION reject_fence_release_${suffix}();
    DROP TRIGGER reject_blob_delete_${suffix} ON file_store_blob;
    DROP FUNCTION reject_blob_delete_${suffix}();
    DELETE FROM resource_lifecycle_references
      WHERE resource_id=${sqlLiteral(ids.late)} AND reference_id='late-postgres-reference';
    DELETE FROM resource_lifecycle_reclamation_fences
      WHERE resource_id=${sqlLiteral(ids.contended)} AND intent_id='external-postgres-reclaimer';
  `);
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

async function publishAgent(endpoint: string): Promise<void> {
  assert.equal((await json('PUT', scoped(WORKSPACE, 'config/providers/resource-e2e'), {
    id: 'resource-e2e', slug: 'resource-e2e', display_name: 'Resource E2E', version: 1,
  })).status, 200);
  assert.equal((await json('PUT', scoped(WORKSPACE, 'config/endpoints/resource-e2e'), {
    id: 'resource-e2e', provider_id: 'resource-e2e', dialect: 'anthropic_messages',
    base_url: `${endpoint}/v1/`, timeout_secs: 10, display_name: 'memory extraction', version: 1,
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
    max_steps: 2, plugins: ['memory'], plugin_config: { memory: {} },
  })).status, 200);
  assert.equal((await json('PUT', scoped(WORKSPACE, `config/agents/${AGENT}/resources`), {
    agent_id: AGENT, revision: 1, inputs: [],
  })).status, 200);
  const published = await json('POST', scoped(WORKSPACE, `config/agents/${AGENT}/publish`));
  assert.equal(published.status, 200, JSON.stringify(published.body));
  const extractor = 'memory-extractor';
  assert.equal((await json('PUT', scoped(WORKSPACE, `config/agents/${extractor}`), {
    name: extractor,
    model: { id: MODEL },
    system: 'You are the memory extraction Agent. Save durable facts.',
    max_steps: 2,
    plugins: [],
    plugin_config: {},
  })).status, 200);
  assert.equal((await json('PUT', scoped(WORKSPACE, `config/agents/${extractor}/resources`), {
    agent_id: extractor,
    revision: 1,
    inputs: [],
  })).status, 200);
  const publishedExtractor = await json(
    'POST',
    scoped(WORKSPACE, `config/agents/${extractor}/publish`),
  );
  assert.equal(publishedExtractor.status, 200, JSON.stringify(publishedExtractor.body));
}

async function main(): Promise<void> {
  const pg = await postgres();
  const upstream = await startFakeAnthropic('resource-e2e-model-key', { behavior: 'memory' });
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
        extraction_policy: { enabled: true },
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

    // Exercise the Postgres v6 identity migration through process replacement:
    // owned rows become catalog aggregates; unowned rows are quarantined; a
    // duplicate cannot replace the already-published canonical config history.
    seedLegacyMemoryIdentities(pg.container, memoryId);

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
    const canonicalMemoryRecord = resourceCatalogRecord(
      pg.container,
      'memory_store',
      memoryId,
    );
    const corruptMemoryRecords = [
      {
        ...structuredClone(canonicalMemoryRecord),
        definition: { ...canonicalMemoryRecord.definition, id: 'forged-memory-id' },
      },
      {
        ...structuredClone(canonicalMemoryRecord),
        definition: { ...canonicalMemoryRecord.definition, workspace_id: '' },
      },
      {
        ...structuredClone(canonicalMemoryRecord),
        definition: { ...canonicalMemoryRecord.definition, current_config_version: 0 },
      },
      (() => {
        const value = structuredClone(canonicalMemoryRecord);
        delete value.configs[String(value.definition.current_config_version)];
        return value;
      })(),
      (() => {
        const value = structuredClone(canonicalMemoryRecord);
        value.configs['2'].memory_store_id = 'forged-memory-id';
        return value;
      })(),
      (() => {
        const value = structuredClone(canonicalMemoryRecord);
        value.configs['2'].version = 99;
        return value;
      })(),
    ];
    for (const corrupt of corruptMemoryRecords) {
      writeResourceCatalogRecord(pg.container, 'memory_store', memoryId, corrupt);
      const failedClosed = await json('GET', scoped(WORKSPACE, 'memory_stores'));
      assert.equal(failedClosed.status, 500, JSON.stringify(failedClosed.body));
      assert.equal(server.exitCode, null, 'catalog corruption must not crash the process');
      writeResourceCatalogRecord(
        pg.container,
        'memory_store',
        memoryId,
        canonicalMemoryRecord,
      );
      assert.equal(
        (await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}`))).status,
        200,
      );
    }
    const legacyPgId = `legacy-pg-owned-${process.pid}`;
    const migratedLegacy = await json('GET', scoped(WORKSPACE, `memory_stores/${legacyPgId}`));
    assert.equal(migratedLegacy.status, 200);
    assert.equal(migratedLegacy.body.name, 'Postgres legacy governed memory');
    assert.equal(
      (await json('GET', scoped(WORKSPACE, `memory_stores/${legacyPgId}/config`))).body.version,
      1,
    );
    assert.equal(
      (await json('GET', scoped(WORKSPACE, `memory_stores/legacy-pg-unowned-${process.pid}`))).status,
      404,
    );
    assert.notEqual(restoredStore.body.name, 'Must not replace canonical Postgres history');
    const patchedLegacy = await json('POST', scoped(WORKSPACE, `memory_stores/${legacyPgId}`), {
      description: 'updated after Postgres migration',
      metadata: { source: 'catalog-v8' },
    });
    assert.equal(patchedLegacy.status, 200);
    assert.equal(patchedLegacy.body.description, 'updated after Postgres migration');
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
    await publishAgent(upstream.url);
    const repository = seedRepository(secondDirectory);
    const session = await json('POST', scoped(WORKSPACE, 'sessions'), {
      agent: AGENT, environment_id: 'env_local',
      resources: [
        {
          type: 'memory_store',
          memory_store_id: memoryId,
          mount_path: '/workspace/memory',
        },
        {
          type: 'github_repository',
          url: repository,
          initial_branch: 'main',
          mount_path: '/workspace/create-time-repository',
        },
      ],
    });
    assert.equal(session.status, 200, JSON.stringify(session.body));
    assert.deepEqual(
      session.body.resources.map((resource: { type: string }) => resource.type),
      ['memory_store', 'github_repository'],
    );
    const turn = await json(
      'POST',
      scoped(WORKSPACE, `sessions/${session.body.id}/events`),
      {
        events: [{
          type: 'user.message',
          content: [{
            type: 'text',
            text: `resolve the governed resource bindings fact-postgres-${process.pid}`,
          }],
        }],
      },
    );
    assert.equal(turn.status, 200, JSON.stringify(turn.body));
    const realized = psql(
      pg.container,
      `SELECT count(*) FROM resource_lifecycle_references WHERE reference_id=${sqlLiteral(session.body.id)}`,
    );
    assert.ok(Number(realized) >= 2, 'the Session activated both frozen resource bindings');
    const extraction = await waitForCompletedExtraction(pg.container, session.body.id);
    assert.equal(extraction.workspace_id, WORKSPACE);
    assert.equal(extraction.memory_store_id, memoryId);
    assert.equal(extraction.memory_config_version, 2);
    assert.equal(extraction.receipt.mutations.length, 1);
    const extractedMemories = await json(
      'GET',
      scoped(WORKSPACE, `memory_stores/${memoryId}/memories`),
    );
    assert.ok(
      extractedMemories.body.data.some(
        (entry: { content: string }) => entry.content.includes(`fact-postgres-${process.pid}`),
      ),
      'the completed Postgres receipt corresponds to content in the bound MemoryStore',
    );

    // A live resource attachment creates a new Session activation generation and
    // therefore revalidates every frozen governed input, including the existing
    // Repository binding. Corrupt the shared Postgres aggregate between requests:
    // no node may infer Repository configuration from the mutable remote, and a
    // failed generation remains pending and contains an error receipt; the SQLite
    // cold-start matrix above proves durable recovery. Here each corruption case is
    // isolated by restoring the captured Postgres Session row between requests.
    const createTimeRepositoryIds = psql(
      pg.container,
      `SELECT id FROM admin_resource_catalog WHERE kind='repository' ` +
        `AND id LIKE ${sqlLiteral(`managed:${session.body.id}:repository:%`)} ORDER BY id`,
    ).split('\n').filter(Boolean);
    assert.equal(
      createTimeRepositoryIds.length,
      1,
      `expected one create-time Repository aggregate: ${JSON.stringify(createTimeRepositoryIds)}`,
    );
    const [createTimeRepositoryId] = createTimeRepositoryIds;
    const canonicalRepositoryRecord = resourceCatalogRecord(
      pg.container,
      'repository',
      createTimeRepositoryId,
    );
    const repositoryConfigVersion = String(
      canonicalRepositoryRecord.definition.current_config_version,
    );
    const stableSessionResources = managedSessionResources(pg.container, session.body.id);
    const corruptRepositoryRecords = [
      {
        ...structuredClone(canonicalRepositoryRecord),
        definition: { ...canonicalRepositoryRecord.definition, id: 'forged-repository-id' },
      },
      {
        ...structuredClone(canonicalRepositoryRecord),
        definition: { ...canonicalRepositoryRecord.definition, workspace_id: '' },
      },
      {
        ...structuredClone(canonicalRepositoryRecord),
        definition: { ...canonicalRepositoryRecord.definition, current_config_version: 0 },
      },
      (() => {
        const value = structuredClone(canonicalRepositoryRecord);
        delete value.configs[repositoryConfigVersion];
        return value;
      })(),
      (() => {
        const value = structuredClone(canonicalRepositoryRecord);
        value.configs[repositoryConfigVersion].repository_id = 'forged-repository-id';
        return value;
      })(),
      (() => {
        const value = structuredClone(canonicalRepositoryRecord);
        value.configs[repositoryConfigVersion].version = 99;
        return value;
      })(),
    ];
    for (const [index, corrupt] of corruptRepositoryRecords.entries()) {
      writeResourceCatalogRecord(
        pg.container,
        'repository',
        createTimeRepositoryId,
        corrupt,
      );
      const denied = await json(
        'POST',
        scoped(WORKSPACE, `sessions/${session.body.id}/resources`),
        {
          type: 'file',
          file_id: fileId,
          mount_path: `/workspace/catalog-probe-${index}.txt`,
        },
      );
      assert.equal(denied.status, 400, `${index}: ${JSON.stringify(denied.body)}`);
      assert.match(JSON.stringify(denied.body), /resource catalog storage failure/u, `${index}`);
      assert.equal(server.exitCode, null, `${index}: Repository corruption crashed the process`);
      const pending = managedSessionResources(pg.container, session.body.id);
      assert.notEqual(pending.pending, undefined, `${index}: failed generation was not durable`);
      assert.equal(pending.activations.at(-1).state, 'prepared', `${index}`);
      assert.equal(pending.activations.at(-1).attempts, 1, `${index}`);
      assert.match(pending.activations.at(-1).last_error, /resource catalog/u, `${index}`);
      writeResourceCatalogRecord(
        pg.container,
        'repository',
        createTimeRepositoryId,
        canonicalRepositoryRecord,
      );
      writeManagedSessionResources(pg.container, session.body.id, stableSessionResources);
    }
    const unchangedResources = await json(
      'GET',
      scoped(WORKSPACE, `sessions/${session.body.id}/resources`),
    );
    assert.equal(unchangedResources.status, 200, JSON.stringify(unchangedResources.body));
    assert.deepEqual(
      unchangedResources.body.data.map((resource: { type: string }) => resource.type),
      ['memory_store', 'github_repository'],
      'recovered probe generations were removed without changing original bindings',
    );

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

    // PostgreSQL distributed-reclaimer fault matrix. These are database-level
    // crash/race injections against production tables, not test-only service APIs.
    const reclamationIds = {
      contended: await upload('postgres contended reclamation'),
      late: await upload('postgres late reference'),
      release: await upload('postgres release failure'),
      physical: await upload('postgres physical failure'),
    };
    installReclamationFaults(pg.container, reclamationIds);
    for (const id of Object.values(reclamationIds)) {
      assert.equal((await json('DELETE', scoped(WORKSPACE, `files/${id}`))).status, 200);
    }
    const faulted = await waitForResourceIntents(
      pg.container,
      Object.values(reclamationIds),
      (intent) => intent.status === 'pending' && intent.attempts >= 1,
    );
    const faultById = new Map(faulted.map((intent) => [intent.target.resource_id, intent]));
    assert.match(faultById.get(reclamationIds.contended).last_error, /fenced by another/u);
    assert.ok(
      faultById.get(reclamationIds.late).blockers.some(
        (blocker: { reference_id: string }) => blocker.reference_id === 'late-postgres-reference',
      ),
    );
    assert.match(faultById.get(reclamationIds.release).last_error, /fence release failure/u);
    assert.match(faultById.get(reclamationIds.physical).last_error, /blob delete failure/u);

    removeReclamationFaults(pg.container, reclamationIds);
    const recoveredFaults = await waitForResourceIntents(
      pg.container,
      Object.values(reclamationIds),
      (intent) => intent.status === 'completed' && intent.receipt !== null,
    );
    assert.ok(recoveredFaults.every((intent) => intent.attempts >= 2));
    assert.equal(
      resourceIntent(pg.container, reclamationIds.release)?.receipt.evidence.blob_deleted,
      false,
      'retry after successful physical delete remains idempotent',
    );
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
    upstream.close();
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
