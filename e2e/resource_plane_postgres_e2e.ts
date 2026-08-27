// Cause/effect E2E for the shared resource persistence axis. Two awaken
// processes use different local directories but one Postgres resource backend;
// File bytes, Memory content/history, Skill bundles, and lifecycle state must all
// survive. Authorization/IAM is deliberately absent from the resource backend.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, type ChildProcess } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import {
  cleanupFixtureTree,
  managedFileUploadForm,
  managedWorkspaceClient,
  spawnProduction,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38436);
const WORKSPACE = `resource-pg-${process.pid}`;
const OTHER_WORKSPACE = `resource-pg-other-${process.pid}`;
const AGENT = `resource-pg-agent-${process.pid}`;
const MODEL = `resource-pg-model-${process.pid}`;
const MANAGED_BETA = 'managed-agents-2026-04-01';
const MEMORY_BETA = 'agent-memory-2026-07-22';
const SKILLS_BETA = 'skills-2025-10-02';
const FILES_BETA = 'files-api-2025-04-14';
const sleep = (milliseconds: number) => new Promise((resolve) => setTimeout(resolve, milliseconds));

function docker(...args: string[]): string {
  return execFileSync('docker', args, { cwd: ROOT, encoding: 'utf8' }).trim();
}

async function postgres(): Promise<{ container: string; url: string; owned: boolean }> {
  const inheritedUrl = process.env.SESSION_DEPLOYMENT_DATABASE_URL;
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

function start(directory: string, databaseUrl: string): ChildProcess {
  return spawnProduction(directory, PORT, {
    workspace: WORKSPACE,
    controlSealKey: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
    databases: {
      resource_database_url: databaseUrl,
      sessions_db: databaseUrl,
      admin_db: databaseUrl,
    },
  });
}

async function ready(): Promise<void> {
  await waitForPort(PORT, 60_000);
}

async function stop(child: ChildProcess): Promise<void> {
  await stopServer(child);
}

const scoped = (workspace: string, suffix: string) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${workspace}/${suffix}`;

async function json(method: string, url: string, body?: unknown) {
  // Protocol-boundary cause/effect table: a Session/Memory/Skill/File URL with
  // its own exact beta reaches the PostgreSQL resource behavior under test;
  // a missing or cross-family beta is rejected before persistence. Other
  // management routes retain the Managed beta. These are exclusive rows, not a
  // combined compatibility header.
  const beta = url.includes('/memory_stores')
    ? MEMORY_BETA
    : url.includes('/skills')
      ? SKILLS_BETA
      : url.includes('/files')
        ? FILES_BETA
        : MANAGED_BETA;
  const response = await fetch(url, {
    method,
    headers: {
      'anthropic-beta': beta,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function upload(content: string, workspace = WORKSPACE): Promise<string> {
  const response = await fetch(scoped(workspace, 'files'), {
    method: 'POST',
    headers: { 'anthropic-beta': FILES_BETA },
    body: managedFileUploadForm(content, 'shared.txt'),
  });
  assert.equal(response.status, 200);
  return (await response.json()).id;
}

async function proveSandboxPolicyPostgresAuthority(): Promise<void> {
  // Causal graph / decision table for the durable Environment policy seam:
  //
  // | exact policy | disabled | current fence | provisioning input | effect |
  // | present      | false    | matches       | omitted            | bind immutable revision and project `eager` |
  // | present      | false    | stale         | any                | reject publish (409)    |
  // | present      | true     | n/a           | any                | reject bind (422)       |
  //
  // This runs through the served API with the production Postgres adapter. It
  // complements the same table's SQLite run instead of creating a store-local
  // test protocol.
  const environment = await json('POST', `http://127.0.0.1:${PORT}/v1/environments`, {
    name: `postgres-policy-${process.pid}`,
    config: { type: 'self_hosted' },
  });
  assert.equal(environment.status, 200, JSON.stringify(environment.body));
  const environmentId = environment.body.id as string;
  const policyId = `postgres-policy-${process.pid}`;
  const policyBase = `http://127.0.0.1:${PORT}/v1/awaken/sandbox-execution-policies`;
  assert.equal((await json('POST', policyBase, {
    id: policyId,
    config: { isolation: 'namespace', limits: { cpu_millis: 500 } },
  })).status, 201);
  assert.equal((await json(
    'POST',
    `http://127.0.0.1:${PORT}/v1/awaken/environments/${environmentId}/sandbox-execution-policy`,
    { policy_id: policyId, version: 1 },
  )).status, 200);
  assert.equal((await json('POST', `${policyBase}/${policyId}/versions`, {
    expected_current: 1,
    config: { isolation: 'container' },
  })).status, 200);
  assert.equal((await json('POST', `${policyBase}/${policyId}/versions`, {
    expected_current: 1,
    config: { isolation: 'workdir' },
  })).status, 409);
  assert.deepEqual(
    (await json(
      'GET',
      `http://127.0.0.1:${PORT}/v1/awaken/environments/${environmentId}/sandbox-execution-policy`,
    )).body,
    { environment_id: environmentId, policy_id: policyId, version: 1, provisioning: 'eager' },
  );

  const disabledId = `${policyId}-disabled`;
  assert.equal((await json('POST', policyBase, {
    id: disabledId,
    config: { isolation: 'workdir' },
    disabled: true,
  })).status, 201);
  assert.equal((await json(
    'POST',
    `http://127.0.0.1:${PORT}/v1/awaken/environments/${environmentId}/sandbox-execution-policy`,
    { policy_id: disabledId, version: 1 },
  )).status, 422);
}

async function uploadSkillVersion(
  route: string,
  marker: string,
  binary?: Uint8Array,
  expectedStatus = 200,
) {
  const form = new FormData();
  form.append(
    'files[]',
    new Blob([
      `---\nname: shared-skill-${process.pid}\ndescription: shared resource test\n---\n${marker}`,
    ], { type: 'text/markdown' }),
    'SKILL.md',
  );
  if (binary !== undefined) {
    const binaryBytes = new Uint8Array(binary.byteLength);
    binaryBytes.set(binary);
    form.append(
      'files[]',
      new Blob([binaryBytes], { type: 'application/octet-stream' }),
      'assets/data.bin',
    );
  }
  const response = await fetch(scoped(WORKSPACE, route), {
    method: 'POST',
    headers: { 'anthropic-beta': SKILLS_BETA },
    body: form,
  });
  const body = await response.json().catch(() => ({}));
  assert.equal(response.status, expectedStatus, `${route}: ${JSON.stringify(body)}`);
  return body;
}

function assertNoLocalResourceTruth(directory: string): void {
  for (const relative of ['files.db', 'memory_fs.db', 'resources.db', 'skills']) {
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
    `SELECT data FROM resource_catalog_entry WHERE kind=${sqlLiteral(kind)} AND id=${sqlLiteral(id)}`,
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
    `UPDATE resource_catalog_entry SET data=${sqlLiteral(JSON.stringify(record))}::jsonb ` +
      `WHERE kind=${sqlLiteral(kind)} AND id=${sqlLiteral(id)}`,
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

function fileBlobId(container: string, fileId: string): string {
  const blobId = psql(
    container,
    `SELECT blob_id FROM file_store_file WHERE id=${sqlLiteral(fileId)}`,
  );
  assert.notEqual(blobId, '', `missing private blob identity for ${fileId}`);
  return blobId;
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

async function publishAgent(endpoint: string, memoryStoreId: string): Promise<void> {
  const connected = await json('POST', scoped(WORKSPACE, 'config/provider-connections'), {
    idempotency_key: 'resource-postgres-provider-connection',
    workspace_id: WORKSPACE,
    provider_id: 'anthropic',
    display_name: 'Resource E2E',
    dialect: 'anthropic_messages',
    base_url: `${endpoint}/v1/`,
    timeout_secs: 10,
    secret: 'resource-e2e-model-key', // awaken-allow: secret
  });
  assert.equal(connected.status, 201, JSON.stringify(connected.body));
  const catalog = await json('GET', scoped(WORKSPACE, 'config/catalog'));
  assert.ok(
    catalog.status === 200
      && Array.isArray(catalog.body.offerings)
      && catalog.body.offerings.some((offering: { model_id?: string }) => offering.model_id === MODEL),
    `Provider Connection discovered ${MODEL}: ${JSON.stringify(catalog.body)}`,
  );
  assert.equal((await json('PUT', scoped(WORKSPACE, `config/agents/${AGENT}`), {
    name: AGENT, model: { id: MODEL }, system: 'Resource lifecycle test.',
    max_steps: 2,
    plugins: ['memory'],
    plugin_config: { memory: { binding_id: 'postgres-memory' } },
  })).status, 200);
  assert.equal((await json('PUT', scoped(WORKSPACE, `config/agents/${AGENT}/resources`), {
    // Binding decision rule: the published plugin selects one explicit Agent
    // binding; a Session resource at the same mount replaces its target while
    // preserving this stable id. No runtime heuristic selects "the" memory.
    agent_id: AGENT,
    revision: 1,
    inputs: [{
      binding_id: 'postgres-memory',
      target: { kind: 'memory_store', id: memoryStoreId },
      mount_path: '/workspace/memory',
      access: 'read_write',
    }],
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
  // Test design (PostgreSQL resource plane). Causes: C1=File/Memory/Skill/policy
  // state is authored in Workspace A; C2=a replacement process uses the same
  // PostgreSQL stores; C3=Workspace B or an archived/corrupt resource is used;
  // C4=a registered Worker realizes the frozen manifest. Effects: E1=all valid
  // state survives restart with exact bytes/versions; E2=C3 is denied without
  // sandbox/model effect; E3=C4 completes one Managed Run and cleanup receipt.
  // Constraints/invariant: PostgreSQL repositories are the sole shared truth;
  // process-local directories and Worker projections never become authorities.
  // Decision rules: G1=C1=>E1; G2=C1+C2=>E1; G3=C3=>E2;
  // G4=C1+C2+C4=>E1+E3.
  const pg = await postgres();
  const upstream = await startFakeAnthropic('resource-e2e-model-key', {
    behavior: 'memory',
    models: [MODEL],
  });
  const firstDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-resource-pg-a-'));
  const secondDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-resource-pg-b-'));
  let server = start(firstDirectory, pg.url);
  try {
    await ready();
    await proveSandboxPolicyPostgresAuthority();
    const fileId = await upload('shared postgres file bytes');
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
    // Awaken-only MemoryStore behavior authoring is intentionally absent from
    // the compatible API; behavior is configured on ordinary Agent plugins.
    assert.equal(
      (await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/config`))).status,
      404,
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
        precondition: { type: 'content_sha256', content_sha256: 'stale' },
      })).status,
      409,
    );
    const updatedMemory = await json(
      'POST',
      scoped(WORKSPACE, `memory_stores/${memoryId}/memories/${memoryEntryId}`),
      {
        path: '/renamed-fact.md',
        content: 'shared postgres memory v2',
        precondition: { type: 'content_sha256', content_sha256: initialSha },
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

    const skill = await uploadSkillVersion('skills', 'Use safely.');
    const skillId = skill.id;
    const duplicateSkill = await uploadSkillVersion('skills', 'duplicate', undefined, 409);
    assert.equal(duplicateSkill.type, 'error');
    const binaryFixture = Uint8Array.from([0, 159, 146, 150, 255, 13, 0, 10]);
    const skillV2 = await uploadSkillVersion(
      `skills/${skillId}/versions`,
      'shared postgres skill v2',
      binaryFixture,
    );
    assert.equal(skillV2.version, '2');
    const skills = await json('GET', scoped(WORKSPACE, 'skills'));
    assert.ok(skills.body.data.some((item: { id: string }) => item.id === skillId));
    console.log('  ok: initial PostgreSQL File, MemoryStore, Skill, and policy state committed');
    await stop(server);
    assertNoLocalResourceTruth(firstDirectory);

    // Cause/effect graph for the single resource authority: C1 the canonical
    // Resources aggregate exists; C2 the retired Control Memory table is absent;
    // C3 the process restarts on a different local directory. C1&&C2&&C3 -> E1
    // canonical history survives unchanged and E2 no legacy/fallback read path
    // can manufacture another MemoryStore. Decision rule R1=[T,T,T]=>[E1,E2].
    // FMECA: retaining or recreating admin_memory_store would establish a second
    // source of truth and could overwrite current history; schema absence plus
    // the post-restart canonical assertions detect that failure mode.
    assert.equal(
      psql(pg.container, "SELECT to_regclass('public.admin_memory_store') IS NULL"),
      't',
      'retired Control Memory storage must not coexist with Resources authority',
    );

    server = start(secondDirectory, pg.url);
    await ready();
    // Persistence/download decision rule: C1 an uploaded input survives process
    // replacement -> E1 its metadata remains readable; C2 downloadable=false ->
    // E2 the public content route stays denied. The later Managed Session mount
    // proves the private bytes are still usable by the governed runtime path.
    const restoredFile = await json('GET', scoped(WORKSPACE, `files/${fileId}`));
    assert.equal(restoredFile.status, 200, 'E1');
    assert.equal(restoredFile.body.size_bytes, 'shared postgres file bytes'.length, 'E1');
    assert.equal(
      (await fetch(scoped(WORKSPACE, `files/${fileId}/content`), {
        headers: { 'anthropic-beta': FILES_BETA },
      })).status,
      400,
      'E2',
    );
    // View decision rule: default/basic redacts content; explicit full is the
    // authorized content-bearing projection used to prove restart durability.
    const memories = await json(
      'GET',
      `${scoped(WORKSPACE, `memory_stores/${memoryId}/memories`)}?view=full`,
    );
    assert.equal(memories.status, 200);
    assert.equal(memories.body.data[0].content, 'shared postgres memory v2');
    assert.equal(memories.body.data[0].path, '/renamed-fact.md');
    const restoredStore = await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}`));
    assert.equal(restoredStore.body.description, 'persisted catalog update');
    assert.deepEqual(restoredStore.body.metadata, { phase: 'updated' });
    assert.equal(
      (await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/config`))).status,
      404,
    );
    const canonicalMemoryRecord = resourceCatalogRecord(
      pg.container,
      'memory_store',
      memoryId,
    );
    const memoryConfigVersion = String(
      canonicalMemoryRecord.definition.current_config_version,
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
        value.configs[memoryConfigVersion].memory_store_id = 'forged-memory-id';
        return value;
      })(),
      (() => {
        const value = structuredClone(canonicalMemoryRecord);
        value.configs[memoryConfigVersion].version = 99;
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
    assert.equal(restoredStore.body.name, 'shared-memory');
    assert.equal(
      (await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/config_versions/1`))).status,
      404,
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
    const skillContent = await fetch(scoped(WORKSPACE, `skills/${skillId}/versions/2/content`), {
      headers: { 'anthropic-beta': SKILLS_BETA },
    });
    assert.equal(skillContent.status, 200);
    assert.match(await skillContent.text(), /shared postgres skill v2/u);
    const skillBinary = await fetch(
      scoped(WORKSPACE, `skills/${skillId}/versions/2/files/assets/data.bin`),
      { headers: { 'anthropic-beta': SKILLS_BETA } },
    );
    assert.equal(skillBinary.status, 200);
    assert.deepEqual(new Uint8Array(await skillBinary.arrayBuffer()), binaryFixture);

    // Workspace routing/ownership is intrinsic resource state. It fails closed
    // without putting a principal, role, token, or policy inside any content port.
    assert.equal((await fetch(scoped(OTHER_WORKSPACE, `files/${fileId}/content`), {
      headers: { 'anthropic-beta': FILES_BETA },
    })).status, 404);
    assert.equal((await json('GET', scoped(OTHER_WORKSPACE, `memory_stores/${memoryId}`))).status, 404);
    assert.equal((await json('GET', scoped(OTHER_WORKSPACE, `skills/${skillId}`))).status, 404);
    console.log('  ok: replacement process restored and isolated all PostgreSQL resource state');

    // Drive the production Managed Session edge so Memory configuration and
    // content use the same PostgreSQL Resource authorities already exercised
    // above. Repository transport has its own HTTPS-focused scenarios and must
    // not be duplicated here with a production-invalid local Git path.
    await publishAgent(upstream.url, memoryId);
    const client = managedWorkspaceClient(`http://127.0.0.1:${PORT}`, WORKSPACE);
    const session = await client.beta.sessions.create({
      agent: AGENT, environment_id: 'env_local',
      resources: [{
        type: 'memory_store',
        memory_store_id: memoryId,
        mount_path: '/workspace/memory',
      }],
      betas: [MANAGED_BETA],
    });
    // Registered-Worker activation cause/effect table. The public Resource list
    // is the canonical desired manifest, while status carries realization state;
    // there is no second installed-resource catalog.
    //
    // | Frozen inputs | Worker acknowledgement | Public resources | Status |
    // |---|---|---|---|
    // | present | absent | both bindings | rescheduling |
    // | present | exact | both bindings | idle/running |
    //
    // Rule R1 covers create and R2 the Run below. FMECA: hiding desired inputs
    // breaks official create/read round-trip; calling them installed invents a
    // parallel projection. Exact resource equality plus the independent status
    // assertion detects both failures.
    assert.deepEqual(
      session.resources.map((resource: { type: string }) => resource.type),
      ['memory_store'],
      'the canonical desired manifest is immediately readable',
    );
    assert.equal(session.status, 'rescheduling');
    // Managed Run decision table: C1=official SDK create froze the MemoryStore;
    // C2=official SDK send returns one exact User receipt; C3=the registered
    // Worker processes C2 and commits a later Agent reply plus idle. E1=C1 is
    // round-trippable; E2=C1+C2+C3 authorizes DB/resource-effect assertions.
    // K: admission or old idle history cannot satisfy C3. Rules P1 !C2=>fail;
    // P2 C2+!C3=>retry for at most 30s; P3 C1+C2+C3=>E1+E2.
    const runReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{
          type: 'text',
          text: `resolve the governed resource bindings fact-postgres-${process.pid}`,
        }],
      }],
      betas: [MANAGED_BETA],
    });
    assert.equal(runReceipt.data?.length, 1, JSON.stringify(runReceipt));
    const runReceiptId = runReceipt.data[0]?.id;
    assert.equal(typeof runReceiptId, 'string', 'P1 exact PostgreSQL Run User receipt');
    await waitForSessionEventReceipt(
      client,
      session.id,
      runReceiptId,
      [MANAGED_BETA],
      ({ delta }: { delta: Array<{ type: string }> }) =>
        delta.some((event) => event.type === 'agent.message')
          && delta.some((event) => event.type === 'session.status_idle'),
      'the PostgreSQL-backed Managed Run to commit its reply and idle edge',
      { timeoutMs: 30_000 },
    );
    const activeSession = await json('GET', scoped(WORKSPACE, `sessions/${session.id}`));
    assert.equal(activeSession.status, 200, JSON.stringify(activeSession.body));
    assert.deepEqual(
      activeSession.body.resources.map((resource: { type: string }) => resource.type),
      ['memory_store'],
    );
    console.log('  ok: registered Worker activated the PostgreSQL-backed MemoryStore input');
    console.log('  ok: Managed Run completed over the PostgreSQL-backed resource bindings');
    const realized = psql(
      pg.container,
      `SELECT count(*) FROM resource_lifecycle_references WHERE reference_id=${sqlLiteral(session.id)}`,
    );
    assert.ok(Number(realized) >= 1, 'the Session activated its frozen MemoryStore binding');
    const extraction = await waitForCompletedExtraction(pg.container, session.id);
    assert.equal(extraction.workspace_id, WORKSPACE);
    assert.equal(extraction.memory_store_id, memoryId);
    assert.equal(extraction.memory_config_version, Number(memoryConfigVersion));
    // Cause graph / decision table for the frozen extractor candidate:
    // C1=published candidate is carried exactly; C2=terminal sub-run succeeds.
    // | Rule | C1 | C2 | result                                      |
    // | R1   | T  | T  | two extractor calls and one durable mutation |
    // | R2   | F  | -  | binding_rejected; no completed extraction    |
    // | R3   | T  | F  | retry/fail closed; no empty success          |
    assert.equal(
      extraction.receipt.mutations.length,
      1,
      JSON.stringify({ extraction, upstreamRequests: upstream.requests }),
    );
    assert.equal(
      upstream.requests.filter((request: { memoryExtractor?: unknown }) => request.memoryExtractor).length,
      2,
      'R1: the exact extractor candidate performs write_memory then its final response',
    );
    const extractedMemories = await json(
      'GET',
      `${scoped(WORKSPACE, `memory_stores/${memoryId}/memories`)}?view=full`,
    );
    assert.ok(
      extractedMemories.body.data.some(
        (entry: { content: string }) => entry.content.includes(`fact-postgres-${process.pid}`),
      ),
      'the completed Postgres receipt corresponds to content in the bound MemoryStore',
    );

    // PostgreSQL workspace-ownership decision table:
    // C1 equal bytes are uploaded in another Workspace; C2 A deletes its logical
    // File; C3 B still owns its distinct logical File. E1 public identities differ,
    // E2 A is denied immediately, E3 B remains readable, and E4 B can revoke its
    // own identity. Physical blob deduplication stays an internal FileStore concern.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // |---|---|---|---|---|
    // | D1 | T | F | T | E1 + B readable |
    // | D2 | T | T | T | E2 + E3 |
    // | D3 | T | T | F | E4 |
    const otherFileId = await upload('shared postgres file bytes', OTHER_WORKSPACE);
    assert.notEqual(otherFileId, fileId, 'D1: logical File identity is workspace-owned');
    assert.equal((await json('GET', scoped(OTHER_WORKSPACE, `files/${otherFileId}`))).status, 200);
    assert.equal((await json('DELETE', scoped(WORKSPACE, `files/${fileId}`))).status, 200);
    assert.equal((await json('GET', scoped(WORKSPACE, `files/${fileId}`))).status, 404);
    assert.equal((await json('GET', scoped(OTHER_WORKSPACE, `files/${otherFileId}`))).status, 200);
    assert.equal((await json('DELETE', scoped(OTHER_WORKSPACE, `files/${otherFileId}`))).status, 200);

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

    // PostgreSQL distributed-reclaimer fault matrix. Public deletion addresses a
    // Workspace-owned logical File, while lifecycle/reclamation owns the private
    // physical blob target. The test must keep those identities distinct just as
    // the production application service does.
    //
    // | Rule | API identity | Lifecycle identity | Effect |
    // |---|---|---|---|
    // | L1 | logical file id | matching blob id | delete creates the exact physical purge intent |
    // | L2 | logical file id | logical file id | forbidden test contract; no such lifecycle target |
    const reclamationFiles = {
      contended: await upload('postgres contended reclamation'),
      late: await upload('postgres late reference'),
      release: await upload('postgres release failure'),
      physical: await upload('postgres physical failure'),
    };
    const reclamationIds = Object.fromEntries(
      Object.entries(reclamationFiles).map(([name, fileId]) => [
        name,
        fileBlobId(pg.container, fileId),
      ]),
    ) as typeof reclamationFiles;
    installReclamationFaults(pg.container, reclamationIds);
    for (const fileId of Object.values(reclamationFiles)) {
      assert.equal((await json('DELETE', scoped(WORKSPACE, `files/${fileId}`))).status, 200);
    }
    const faulted = await waitForResourceIntents(
      pg.container,
      Object.values(reclamationIds),
      (intent) => intent.status === 'pending' && intent.attempts >= 1,
    );
    const faultById = new Map(faulted.map((intent) => [intent.target.resource_id, intent]));
    const contendedFault = faultById.get(reclamationIds.contended);
    const lateFault = faultById.get(reclamationIds.late);
    const releaseFault = faultById.get(reclamationIds.release);
    const physicalFault = faultById.get(reclamationIds.physical);
    assert.ok(contendedFault && lateFault && releaseFault && physicalFault);
    assert.match(contendedFault.last_error, /fenced by another/u);
    assert.ok(
      lateFault.blockers.some(
        (blocker: { reference_id: string }) => blocker.reference_id === 'late-postgres-reference',
      ),
    );
    assert.match(releaseFault.last_error, /fence release failure/u);
    assert.match(physicalFault.last_error, /blob delete failure/u);

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
    // The first process realizes a writable Memory projection. Whether teardown
    // completed or the process died determines ordinary removal vs deepest-first
    // detach; both directory owners use the canonical cleanup decision table.
    cleanupFixtureTree(firstDirectory);
    cleanupFixtureTree(secondDirectory);
    if (pg.owned) {
      try { docker('rm', '-f', pg.container); } catch { /* best effort */ }
    }
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
