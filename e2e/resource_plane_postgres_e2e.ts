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

async function upload(content: string): Promise<string> {
  const form = new FormData();
  form.append('purpose', 'agent');
  form.append('file', new Blob([content]), 'shared.txt');
  const response = await fetch(scoped(WORKSPACE, 'files'), { method: 'POST', body: form });
  assert.equal(response.status, 200);
  return (await response.json()).id;
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

async function main(): Promise<void> {
  const pg = await postgres();
  const firstDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-resource-pg-a-'));
  const secondDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-resource-pg-b-'));
  const bin = binary();
  let server = start(bin, firstDirectory, pg.url);
  try {
    await ready();
    const fileId = await upload('shared postgres file bytes');
    const memoryStore = await json('POST', scoped(WORKSPACE, 'memory_stores'), { name: 'shared-memory' });
    assert.equal(memoryStore.status, 200);
    const memoryId = memoryStore.body.id;
    const memory = await json('POST', scoped(WORKSPACE, `memory_stores/${memoryId}/memories`), {
      path: '/fact.md',
      content: 'shared postgres memory',
    });
    assert.equal(memory.status, 200);
    const skill = await json('POST', scoped(WORKSPACE, 'skills'), {
      id: `shared-skill-${process.pid}`,
      content: `---\nname: shared-skill-${process.pid}\ndescription: shared resource test\n---\nUse safely.`,
    });
    assert.equal(skill.status, 200);
    const skillId = skill.body.id;
    await stop(server);
    assertNoLocalResourceTruth(firstDirectory);

    server = start(bin, secondDirectory, pg.url);
    await ready();
    const fileResponse = await fetch(scoped(WORKSPACE, `files/${fileId}/content`));
    assert.equal(fileResponse.status, 200);
    assert.equal(await fileResponse.text(), 'shared postgres file bytes');
    const memories = await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/memories`));
    assert.equal(memories.status, 200);
    assert.equal(memories.body.data[0].content, 'shared postgres memory');
    const versions = await json('GET', scoped(WORKSPACE, `memory_stores/${memoryId}/memory_versions`));
    assert.equal(versions.status, 200);
    assert.equal(versions.body.data.length, 1);
    assert.equal((await json('GET', scoped(WORKSPACE, `skills/${skillId}`))).status, 200);

    // Workspace routing/ownership is intrinsic resource state. It fails closed
    // without putting a principal, role, token, or policy inside any content port.
    assert.equal((await fetch(scoped(OTHER_WORKSPACE, `files/${fileId}/content`))).status, 404);
    assert.equal((await json('GET', scoped(OTHER_WORKSPACE, `memory_stores/${memoryId}`))).status, 404);
    assert.equal((await json('GET', scoped(OTHER_WORKSPACE, `skills/${skillId}`))).status, 404);
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
