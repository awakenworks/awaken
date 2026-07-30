// Real-process resource lifecycle for `awaken` no-login/no-storage mode. This
// drives the production ephemeral adapter family (in-memory File/Skill/lifecycle
// plus VolatileMemoryRepository) through public HTTP. IAM is absent; the
// composition root injects one explicit platform Workspace and resource stores
// enforce ownership/state independently.

import assert from 'node:assert/strict';
import { spawnServer, stopServer, waitForPort } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38437);
const WORKSPACE = `ephemeral-resource-${process.pid}`;
const OTHER = `ephemeral-resource-other-${process.pid}`;
const BETAS = 'managed-agents-2026-04-01,files-api-2025-04-14';
const sleep = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

function start() {
  return spawnServer('resource-ephemeral', PORT).server;
}

async function ready(child) {
  await waitForPort(PORT, 60_000, child);
}

async function stop(child) {
  await stopServer(child);
}

const scoped = (workspace, tail) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${workspace}/${tail}`;

async function json(method, workspace, tail, body) {
  const response = await fetch(scoped(workspace, tail), {
    method,
    headers: {
      'anthropic-beta': BETAS,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function upload(workspace, content) {
  const form = new FormData();
  form.append('purpose', 'agent');
  form.append('file', new Blob([content]), 'input.txt');
  const response = await fetch(scoped(workspace, 'files'), {
    method: 'POST',
    headers: { 'anthropic-beta': BETAS },
    body: form,
  });
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

    // The no-storage composition still runs the same authorization-independent
    // lifecycle state machine over its in-memory adapter. A live Session edge
    // must defer physical reclamation; archiving removes the edge and lets the
    // background reconciler converge without a durable database.
    const heldFile = await upload(WORKSPACE, 'ephemeral session-held bytes');
    const heldSession = await json('POST', WORKSPACE, 'sessions', {
      agent: 'assistant',
      environment_id: 'env_local',
    });
    assert.equal(heldSession.status, 200, JSON.stringify(heldSession.body));
    assert.equal(
      (await json('POST', WORKSPACE, `sessions/${heldSession.body.id}/resources`, {
        type: 'file',
        file_id: heldFile.id,
        mount_path: '/workspace/held.txt',
      })).status,
      200,
    );
    assert.equal((await json('DELETE', WORKSPACE, `files/${heldFile.id}`)).status, 200);
    await sleep(5_500);
    assert.equal(
      (await json('POST', WORKSPACE, `sessions/${heldSession.body.id}/archive`)).status,
      200,
    );

    const createdStore = await json('POST', WORKSPACE, 'memory_stores', { name: 'volatile' });
    assert.equal(createdStore.status, 200);
    const store = createdStore.body.id;
    assert.equal((await json('GET', WORKSPACE, 'memory_stores/missing/config')).status, 404);
    assert.equal(
      (await json('GET', WORKSPACE, `memory_stores/${store}/config_versions/not-a-number`)).status,
      400,
    );
    assert.equal(
      (await json('GET', WORKSPACE, `memory_stores/${store}/config_versions/99`)).status,
      404,
    );
    assert.equal(
      (await json('POST', WORKSPACE, `memory_stores/${store}/config`, {
        recall_policy: { enabled: true },
      })).status,
      400,
    );
    assert.equal(
      (await json('POST', WORKSPACE, `memory_stores/${store}/config`, {
        expected_config_version: 1,
      })).status,
      400,
    );
    for (const invalidPolicy of [
      { recall_policy: 'invalid' },
      { extraction_policy: 'invalid' },
      { retention_policy: 'invalid' },
    ]) {
      assert.equal(
        (await json('POST', WORKSPACE, `memory_stores/${store}/config`, {
          expected_config_version: 1,
          ...invalidPolicy,
        })).status,
        400,
      );
    }
    assert.equal(
      (await json('POST', WORKSPACE, `memory_stores/${store}/config`, {
        expected_config_version: Number.MAX_SAFE_INTEGER,
        recall_policy: { enabled: true },
      })).status,
      409,
      'a stale large CAS base conflicts without changing configuration',
    );
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
    const prefix = await json('GET', WORKSPACE, `memory_stores/${store}/memories?path_prefix=/notes/`);
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
      (await json('POST', WORKSPACE, `memory_stores/${store}/memories/missing-memory`, {
        content: 'cannot update an absent head',
      })).status,
      404,
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
    assert.equal(
      (await json('POST', WORKSPACE, `memory_stores/${store}/config`, {
        expected_config_version: 1,
        recall_policy: { enabled: false },
      })).status,
      409,
      'an archived store cannot publish another behavior version',
    );

    // Skill is an independently durable aggregate. Ephemeral ResourcePlane
    // composition must not invent a parallel volatile Skill implementation.
    const skillId = `volatile-skill-${process.pid}`;
    const skill = await json('POST', WORKSPACE, 'skills', {
      id: skillId,
      content: `---\nname: ${skillId}\ndescription: ephemeral\n---\nUse safely.`,
    });
    assert.equal(skill.status, 409, JSON.stringify(skill.body));
    assert.match(skill.body.error, /no durable skill store/u);

    assert.equal((await json('DELETE', OTHER, `files/${file.id}`)).status, 200);
    await sleep(5_500);
    console.log('E2E PASS: ephemeral resource adapters are scoped, lifecycle-complete, and do not synthesize Skill durability.');
  } finally {
    await stop(server);
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
