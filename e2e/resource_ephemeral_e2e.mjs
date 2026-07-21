// Real-process resource lifecycle for `awaken` no-login/no-storage mode. This
// drives the production ephemeral adapter family (in-memory File/Skill/lifecycle
// plus VolatileMemoryRepository) through public HTTP. IAM is absent; the
// composition root injects one explicit platform Workspace and resource stores
// enforce ownership/state independently.

import assert from 'node:assert/strict';
import net from 'node:net';
import path from 'node:path';
import { execSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38437);
const WORKSPACE = `ephemeral-resource-${process.pid}`;
const OTHER = `ephemeral-resource-other-${process.pid}`;

function binary() {
  const output = execSync('cargo build --quiet --message-format=json -p awaken-cli --bin awaken', {
    cwd: ROOT,
    env: process.env,
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

function start() {
  const environment = { ...process.env };
  for (const key of [
    'AWAKEN_DEPLOYMENT_DATA_DIR',
    'AWAKEN_MGMT_DIR',
    'AWAKEN_RESOURCE_DATABASE_URL',
    'AWAKEN_RESOURCE_LIFECYCLE_DB',
    'AWAKEN_ADMIN_DB',
    'AWAKEN_MGMT_IAM',
    'AWAKEN_IDENTITY_MODE',
  ]) delete environment[key];
  return spawn(binary(), {
    env: {
      ...environment,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_LOCAL_WORKSPACE_ID: WORKSPACE,
    },
    stdio: ['ignore', 'ignore', 'inherit'],
  });
}

async function ready(child) {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const connected = await new Promise((resolve) => {
      const socket = net.createConnection({ host: '127.0.0.1', port: PORT });
      socket.once('connect', () => { socket.destroy(); resolve(true); });
      socket.once('error', () => { socket.destroy(); resolve(false); });
    });
    if (connected) return;
    if (child.exitCode !== null) throw new Error(`awaken exited with ${child.exitCode}`);
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error('awaken did not become ready');
}

async function stop(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  child.kill('SIGINT');
  await new Promise((resolve) => child.once('exit', resolve));
}

const scoped = (workspace, tail) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${workspace}/${tail}`;

async function json(method, workspace, tail, body) {
  const response = await fetch(scoped(workspace, tail), {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function upload(workspace, content) {
  const form = new FormData();
  form.append('purpose', 'agent');
  form.append('file', new Blob([content]), 'input.txt');
  const response = await fetch(scoped(workspace, 'files'), { method: 'POST', body: form });
  assert.equal(response.status, 200);
  return response.json();
}

async function main() {
  const server = start();
  try {
    await ready(server);

    const file = await upload(WORKSPACE, 'same immutable bytes');
    assert.equal((await upload(WORKSPACE, 'same immutable bytes')).id, file.id);
    assert.equal((await upload(OTHER, 'same immutable bytes')).id, file.id);
    assert.equal((await json('GET', WORKSPACE, `files/${file.id}`)).body.id, file.id);
    assert.equal(await (await fetch(scoped(WORKSPACE, `files/${file.id}/content`))).text(), 'same immutable bytes');
    assert.equal((await json('DELETE', WORKSPACE, `files/${file.id}`)).status, 200);
    assert.equal((await fetch(scoped(WORKSPACE, `files/${file.id}/content`))).status, 404);
    assert.equal((await fetch(scoped(OTHER, `files/${file.id}/content`))).status, 200);

    const createdStore = await json('POST', WORKSPACE, 'memory_stores', { name: 'volatile' });
    assert.equal(createdStore.status, 200);
    const store = createdStore.body.id;
    for (const invalid of ['relative.md', '/', '/a/../b.md', '/a//b.md', '/a/./b.md']) {
      assert.equal(
        (await json('POST', WORKSPACE, `memory_stores/${store}/memories`, {
          path: invalid,
          content: 'invalid',
        })).status,
        400,
        `invalid path ${invalid}`,
      );
    }
    const first = await json('POST', WORKSPACE, `memory_stores/${store}/memories`, {
      path: '/notes/a.md', content: 'alpha',
    });
    assert.equal(first.status, 200);
    assert.equal(
      (await json('POST', WORKSPACE, `memory_stores/${store}/memories`, {
        path: '/notes/a.md', content: 'duplicate',
      })).status,
      409,
    );
    await json('POST', WORKSPACE, `memory_stores/${store}/memories`, {
      path: '/notes/b.md', content: 'displaced',
    });
    await json('POST', WORKSPACE, `memory_stores/${store}/memories`, {
      path: '/root.md', content: 'root',
    });
    const prefix = await json('GET', WORKSPACE, `memory_stores/${store}/memories?path_prefix=/notes`);
    assert.deepEqual(prefix.body.data.map((entry) => entry.path), ['/notes/a.md', '/notes/b.md']);
    const versionsBeforeRejectedUpdate = await json(
      'GET', WORKSPACE, `memory_stores/${store}/memory_versions`,
    );
    assert.equal(
      (await json('POST', WORKSPACE, `memory_stores/${store}/memories/${first.body.id}`, {
        content: 'must-not-commit',
        path: 'relative.md',
        precondition: { content_sha256: first.body.content_sha256 },
      })).status,
      400,
    );
    const afterInvalidPath = await json(
      'GET', WORKSPACE, `memory_stores/${store}/memories/${first.body.id}`,
    );
    assert.equal(afterInvalidPath.body.path, '/notes/a.md');
    assert.equal(afterInvalidPath.body.content, 'alpha');
    assert.equal(
      (await json('GET', WORKSPACE, `memory_stores/${store}/memory_versions`)).body.data.length,
      versionsBeforeRejectedUpdate.body.data.length,
      'invalid target path rolls back content and history',
    );
    assert.equal(
      (await json('POST', WORKSPACE, `memory_stores/${store}/memories/${first.body.id}`, {
        content: 'alpha',
        path: '/notes/stale-move.md',
        precondition: { content_sha256: 'stale' },
      })).status,
      409,
    );
    assert.equal(
      (await json('GET', WORKSPACE, `memory_stores/${store}/memories/${first.body.id}`)).body.path,
      '/notes/a.md',
      'matching content cannot make a stale path mutation idempotent',
    );
    const updated = await json('POST', WORKSPACE, `memory_stores/${store}/memories/${first.body.id}`, {
      content: 'beta',
      path: '/notes/b.md',
      precondition: { content_sha256: first.body.content_sha256 },
    });
    assert.equal(updated.status, 200);
    assert.equal(updated.body.path, '/notes/b.md');
    assert.equal(updated.body.content, 'beta');
    assert.equal(
      (await json('GET', WORKSPACE, `memory_stores/${store}/memory_versions`)).body.data.length,
      versionsBeforeRejectedUpdate.body.data.length + 2,
      'rename-replace appends displaced delete plus one combined head update',
    );
    const idempotent = await json('POST', WORKSPACE, `memory_stores/${store}/memories/${first.body.id}`, {
      content: 'beta', precondition: { content_sha256: 'stale' },
    });
    assert.equal(idempotent.status, 200);
    assert.equal(idempotent.body.memory_version_id, updated.body.memory_version_id);
    const versions = await json('GET', WORKSPACE, `memory_stores/${store}/memory_versions`);
    assert.ok(versions.body.data.some((version) => version.operation === 'deleted'));
    assert.equal((await json('GET', OTHER, `memory_stores/${store}`)).status, 404);
    assert.equal((await json('DELETE', WORKSPACE, `memory_stores/${store}/memories/${first.body.id}`)).status, 200);
    assert.equal((await json('DELETE', WORKSPACE, `memory_stores/${store}`)).status, 200);

    const skillId = `volatile-skill-${process.pid}`;
    const skill = await json('POST', WORKSPACE, 'skills', {
      id: skillId,
      content: `---\nname: ${skillId}\ndescription: ephemeral\n---\nUse safely.`,
    });
    assert.equal(skill.status, 200);
    assert.equal((await json('GET', OTHER, `skills/${skillId}`)).status, 404);
    assert.equal((await json('DELETE', WORKSPACE, `skills/${skillId}`)).status, 200);

    assert.equal((await json('DELETE', OTHER, `files/${file.id}`)).status, 200);
    console.log('E2E PASS: production ephemeral resource adapters are scoped and lifecycle-complete.');
  } finally {
    await stop(server);
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
