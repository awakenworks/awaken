// Real-process proof that resource adapters never infer a Workspace. This starts
// the test-only raw resource-router composition without the production local
// Workspace injector or a cloud PEP; every route must fail before resource IO.

import assert from 'node:assert/strict';
import net from 'node:net';
import path from 'node:path';
import { execSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38438);

function binary() {
  const output = execSync(
    'cargo build --quiet --message-format=json -p awaken-scenario-host --bin awaken-scenario-host',
    { cwd: ROOT, env: process.env, maxBuffer: 64 * 1024 * 1024 },
  ).toString();
  for (const line of output.split('\n')) {
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken-scenario-host') {
        return message.executable;
      }
    } catch { /* cargo diagnostic */ }
  }
  throw new Error('awaken-scenario-host binary was not produced');
}

function start() {
  return spawn(binary(), {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_MODEL_MODE: 'resource-scope-boundary',
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
    if (child.exitCode !== null) throw new Error(`scenario host exited with ${child.exitCode}`);
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error('scenario host did not become ready');
}

async function stop(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  child.kill('SIGINT');
  await new Promise((resolve) => child.once('exit', resolve));
}

async function expectMissingWorkspace(method, pathname, body, contentType = 'application/json') {
  const response = await fetch(`http://127.0.0.1:${PORT}${pathname}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': contentType },
    body,
  });
  assert.equal(response.status, 404, `${method} ${pathname}`);
  const payload = await response.json();
  assert.equal(payload.error, 'workspace not found', `${method} ${pathname}`);
}

async function main() {
  const server = start();
  try {
    await ready(server);
    for (const pathname of [
      '/v1/files',
      '/v1/files/file-unknown',
      '/v1/memory_stores',
      '/v1/memory_stores/store-unknown',
      '/v1/skills',
      '/v1/skills/skill-unknown',
    ]) {
      await expectMissingWorkspace('GET', pathname);
    }

    await expectMissingWorkspace('POST', '/v1/files', 'not-a-multipart-body', 'text/plain');
    await expectMissingWorkspace('POST', '/v1/memory_stores', JSON.stringify({ name: 'blocked' }));
    await expectMissingWorkspace(
      'POST',
      '/v1/skills',
      JSON.stringify({ id: 'blocked', content: 'must not persist' }),
    );
    console.log('E2E PASS: resource adapters require a preselected Workspace and fail closed.');
  } finally {
    await stop(server);
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
