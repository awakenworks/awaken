// Cause-effect E2E for ADR-0063 durable resource reclamation through the real
// `awaken` composition root. It covers process death after logical delete,
// shared-File retention, Memory head+history purge, Skill tombstone purge, and
// durable receipts. IAM is intentionally not queried by the background worker.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, execSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38435);
const WS_A = `reclaim-a-${process.pid}`;
const WS_B = `reclaim-b-${process.pid}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function binary() {
  const output = execSync('cargo build --quiet --message-format=json -p awaken-cli --bin awaken', {
    cwd: ROOT,
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

function start(bin, directory) {
  const child = spawn(bin, {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_LOCAL_WORKSPACE_ID: WS_A,
      AWAKEN_STORAGE_DIR: directory,
      AWAKEN_MGMT_DIR: directory,
      AWAKEN_MGMT_SEAL_KEY: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
    },
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  return child;
}

async function ready() {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const socketReady = await new Promise((resolve) => {
      const socket = net.createConnection({ port: PORT, host: '127.0.0.1' });
      socket.once('connect', () => { socket.destroy(); resolve(true); });
      socket.once('error', () => { socket.destroy(); resolve(false); });
    });
    if (socketReady) return;
    await sleep(100);
  }
  throw new Error('awaken did not become ready');
}

async function stop(child, signal = 'SIGINT') {
  if (child.exitCode !== null) return;
  child.kill(signal);
  await new Promise((resolve) => child.once('exit', resolve));
}

const scoped = (workspace, suffix) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${workspace}/${suffix}`;

async function json(method, url, body) {
  const response = await fetch(url, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function upload(workspace, content, filename = 'input.txt') {
  const form = new FormData();
  form.append('purpose', 'agent');
  form.append('file', new Blob([content]), filename);
  const response = await fetch(scoped(workspace, 'files'), { method: 'POST', body: form });
  assert.equal(response.status, 200);
  return (await response.json()).id;
}

function receipts(directory) {
  const database = path.join(directory, 'resource-lifecycle.db');
  if (!fs.existsSync(database)) return [];
  const raw = execFileSync('sqlite3', ['-json', database, 'SELECT data FROM resource_purge_intents'])
    .toString()
    .trim();
  return raw ? JSON.parse(raw).map((row) => JSON.parse(row.data)) : [];
}

async function waitReceipt(directory, kind, resourceId, timeoutMs = 20_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const receipt = receipts(directory).find(
      (intent) => intent.target.kind === kind
        && intent.target.resource_id === resourceId
        && intent.status === 'completed',
    );
    if (receipt) return receipt;
    await sleep(200);
  }
  throw new Error(`no completed ${kind}/${resourceId} purge receipt`);
}

function scalar(database, sql) {
  return Number(execFileSync('sqlite3', [database, sql]).toString().trim());
}

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-resource-reclaim-'));
  const bin = binary();
  let server = start(bin, directory);
  try {
    await ready();

    // Crash after the API committed durable intent + logical revoke. Startup
    // recovery must finish the same intent without another delete request.
    const crashedFile = await upload(WS_A, 'crash-recovery');
    assert.equal((await json('DELETE', scoped(WS_A, `files/${crashedFile}`))).status, 200);
    await stop(server, 'SIGKILL');
    server = start(bin, directory);
    await ready();
    const crashReceipt = await waitReceipt(directory, 'file', crashedFile);
    assert.equal(crashReceipt.receipt.evidence.blob_deleted, true);
    assert.equal(
      scalar(path.join(directory, 'files.db'),
        `SELECT count(*) FROM file_store_blob WHERE id='${crashedFile}'`),
      0,
    );

    // Equal bytes share one blob. Revoking A cannot delete bytes still granted
    // to B; revoking B subsequently permits physical GC.
    const sharedA = await upload(WS_A, 'shared-content');
    const sharedB = await upload(WS_B, 'shared-content');
    assert.equal(sharedA, sharedB);
    assert.equal((await json('DELETE', scoped(WS_A, `files/${sharedA}`))).status, 200);
    await sleep(5_500);
    assert.equal((await fetch(scoped(WS_B, `files/${sharedB}/content`))).status, 200);
    assert.equal((await json('DELETE', scoped(WS_B, `files/${sharedB}`))).status, 200);
    await waitReceipt(directory, 'file', sharedB);

    // Memory purge removes live heads and the version log from the same canonical
    // repository only after the store tombstone is visible.
    const memoryStore = await json('POST', scoped(WS_A, 'memory_stores'), { name: 'reclaim-me' });
    assert.equal(memoryStore.status, 200);
    const memoryId = memoryStore.body.id;
    assert.equal((await json('POST', scoped(WS_A, `memory_stores/${memoryId}/memories`), {
      path: '/fact.md', content: 'remember',
    })).status, 200);
    assert.equal((await json('DELETE', scoped(WS_A, `memory_stores/${memoryId}`))).status, 200);
    const memoryReceipt = await waitReceipt(directory, 'memory_store', memoryId);
    assert.equal(memoryReceipt.receipt.evidence.heads_deleted, 1);
    assert.equal(memoryReceipt.receipt.evidence.versions_deleted, 1);

    // Skill delete hides new resolution immediately, then removes the retained
    // immutable bundle once no Session/Agent binding remains.
    const skill = await json('POST', scoped(WS_A, 'skills'), {
      id: 'reclaim-skill',
      content: '---\nname: reclaim-skill\ndescription: test\n---\nUse safely.',
    });
    assert.equal(skill.status, 200);
    const skillId = skill.body.id;
    assert.equal((await json('DELETE', scoped(WS_A, `skills/${skillId}`))).status, 200);
    assert.equal((await json('GET', scoped(WS_A, `skills/${skillId}`))).status, 404);
    const skillReceipt = await waitReceipt(directory, 'skill', skillId);
    assert.equal(skillReceipt.receipt.evidence.versions_deleted, 1);

    console.log('E2E PASS: durable resource deny, crash recovery, reference guards, and per-kind receipts.');
  } finally {
    await stop(server).catch(() => {});
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
