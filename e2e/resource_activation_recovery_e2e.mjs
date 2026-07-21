// Cause-effect E2E for durable Session resource activation recovery. The test
// stops the production `awaken` process at the two persisted crash windows and
// proves startup reconciliation converges without an IAM/policy dependency.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, execSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38436);
const WORKSPACE = `activation-recovery-${process.pid}`;
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
  return spawn(bin, {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_LOCAL_WORKSPACE_ID: WORKSPACE,
      AWAKEN_STORAGE_DIR: directory,
      AWAKEN_DEPLOYMENT_DATA_DIR: directory,
      AWAKEN_MGMT_SEAL_KEY: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
    },
    stdio: ['ignore', 'ignore', 'inherit'],
  });
}

async function ready() {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const connected = await new Promise((resolve) => {
      const socket = net.createConnection({ port: PORT, host: '127.0.0.1' });
      socket.once('connect', () => { socket.destroy(); resolve(true); });
      socket.once('error', () => { socket.destroy(); resolve(false); });
    });
    if (connected) return;
    await sleep(100);
  }
  throw new Error('awaken did not become ready');
}

async function stop(child, signal = 'SIGINT') {
  if (child.exitCode !== null) return;
  child.kill(signal);
  await new Promise((resolve) => child.once('exit', resolve));
}

const scoped = (suffix) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${WORKSPACE}/${suffix}`;

async function json(method, url, body) {
  const response = await fetch(url, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function upload(content, filename) {
  const form = new FormData();
  form.append('purpose', 'agent');
  form.append('file', new Blob([content]), filename);
  const response = await fetch(scoped('files'), { method: 'POST', body: form });
  assert.equal(response.status, 200);
  return (await response.json()).id;
}

function seedRepository(root) {
  const work = path.join(root, 'activation-repository-work');
  const remote = path.join(root, 'activation-repository.git');
  fs.mkdirSync(work, { recursive: true });
  execFileSync('git', ['init', '-q', '-b', 'main'], { cwd: work });
  execFileSync('git', ['config', 'user.email', 'activation@example.invalid'], { cwd: work });
  execFileSync('git', ['config', 'user.name', 'activation-recovery'], { cwd: work });
  fs.writeFileSync(path.join(work, 'README.md'), 'activation recovery');
  execFileSync('git', ['add', 'README.md'], { cwd: work });
  execFileSync('git', ['commit', '-q', '-m', 'seed'], { cwd: work });
  execFileSync('git', ['clone', '-q', '--bare', work, remote]);
  return remote;
}

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function sessionRow(database, sessionId) {
  const output = execFileSync('sqlite3', [
    '-json',
    database,
    `SELECT status, archived_at, effective_inputs_json FROM managed_session WHERE session_id=${sqlQuote(sessionId)}`,
  ]).toString().trim();
  const rows = output ? JSON.parse(output) : [];
  assert.equal(rows.length, 1, `missing durable Session ${sessionId}`);
  return {
    status: rows[0].status,
    archivedAt: rows[0].archived_at,
    resources: JSON.parse(rows[0].effective_inputs_json),
  };
}

function updateSessionRow(database, sessionId, status, archivedAt, resources) {
  execFileSync('sqlite3', [
    database,
    `UPDATE managed_session SET status=${sqlQuote(status)}, archived_at=${
      archivedAt === null ? 'NULL' : sqlQuote(archivedAt)
    }, effective_inputs_json=${sqlQuote(JSON.stringify(resources))} WHERE session_id=${sqlQuote(sessionId)}`,
  ]);
}

function persistPreparedGeneration(database, sessionId) {
  const row = sessionRow(database, sessionId);
  assert.equal(row.status, 'idle');
  assert.equal(row.resources.pending, undefined);
  const revision = row.resources.revision + 1;
  const previous = row.resources.activations.map((activation) => ({
    ...activation,
    state: activation.state === 'active' ? 'releasing' : activation.state,
  }));
  const prepared = row.resources.activations
    .filter((activation) => activation.state === 'active')
    .map((activation) => ({
      ...activation,
      activation_id: `${sessionId}:${revision}:${activation.binding_id}`,
      revision,
      state: 'prepared',
      attempts: 0,
      last_error: 'process died before realization',
    }));
  assert.ok(prepared.length > 0);
  updateSessionRow(database, sessionId, 'idle', null, {
    revision,
    active: row.resources.active,
    pending: row.resources.active,
    activations: [...previous, ...prepared],
  });
}

function persistTerminalRelease(database, sessionId) {
  const row = sessionRow(database, sessionId);
  assert.equal(row.status, 'idle');
  const resources = {
    ...row.resources,
    activations: row.resources.activations.map((activation) => ({
      ...activation,
      state: activation.state === 'active' ? 'releasing' : activation.state,
    })),
  };
  assert.ok(resources.activations.some((activation) => activation.state === 'releasing'));
  updateSessionRow(database, sessionId, 'terminated', '2026-07-22T00:00:00Z', resources);
}

function receipts(directory) {
  const database = path.join(directory, 'resource-lifecycle.db');
  if (!fs.existsSync(database)) return [];
  const output = execFileSync('sqlite3', [
    '-json',
    database,
    'SELECT data FROM resource_purge_intents',
  ]).toString().trim();
  return output ? JSON.parse(output).map((row) => JSON.parse(row.data)) : [];
}

async function waitRepositoryReceipt(directory, resourceId) {
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    const receipt = receipts(directory).find((intent) =>
      intent.target.kind === 'repository'
      && intent.target.resource_id === resourceId
      && intent.status === 'completed');
    if (receipt) return receipt;
    await sleep(200);
  }
  throw new Error(`no completed Repository receipt for ${resourceId}`);
}

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-activation-recovery-'));
  const sessionsDatabase = path.join(directory, 'sessions.db');
  const bin = binary();
  let server = start(bin, directory);
  try {
    await ready();

    const malformed = await json('POST', scoped('sessions'), {
      agent: 'assistant',
      resources: [{ type: 'unsupported-resource' }],
    });
    assert.equal(malformed.status, 400);

    const fileId = await upload('recover this binding', 'recovery.txt');
    const recovering = await json('POST', scoped('sessions'), {
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{ type: 'file', file_id: fileId, mount_path: '/workspace/recovery.txt' }],
    });
    assert.equal(recovering.status, 200, JSON.stringify(recovering.body));

    const repository = seedRepository(directory);
    const terminating = await json('POST', scoped('sessions'), {
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{
        type: 'github_repository',
        url: repository,
        mount_path: '/workspace/terminal-repository',
      }],
    });
    assert.equal(terminating.status, 200, JSON.stringify(terminating.body));

    // Model process death after each first durable edge: Prepared for a live
    // replacement, and Releasing for a terminal Session. These are precisely
    // the states the coordinator persists before external sandbox/catalog IO.
    await stop(server, 'SIGKILL');
    persistPreparedGeneration(sessionsDatabase, recovering.body.id);
    persistTerminalRelease(sessionsDatabase, terminating.body.id);

    server = start(bin, directory);
    await ready();

    const recovered = sessionRow(sessionsDatabase, recovering.body.id);
    assert.equal(recovered.status, 'idle');
    assert.equal(recovered.resources.pending, undefined);
    assert.deepEqual(
      recovered.resources.activations.map((activation) => activation.state),
      ['released', 'active'],
    );
    assert.equal(recovered.resources.activations[1].attempts, 1);
    assert.equal(recovered.resources.activations[1].last_error, undefined);

    const released = sessionRow(sessionsDatabase, terminating.body.id);
    assert.equal(released.status, 'terminated');
    assert.equal(released.resources.pending, undefined);
    assert.ok(released.resources.activations.every((activation) => activation.state === 'released'));

    const repositoryId = `managed:${terminating.body.id}:repository:0`;
    const receipt = await waitRepositoryReceipt(directory, repositoryId);
    assert.equal(receipt.receipt.evidence.local_realizations_deleted, 0);

    console.log('E2E PASS: Prepared activation and terminal release recover after process death.');
  } finally {
    await stop(server).catch(() => {});
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
