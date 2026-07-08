// Shared e2e harness: spawn awaken-server-local in a chosen model mode, wait for
// it to listen, run a body, and always shut it down. Model modes are the
// deterministic stub models (no API key): `echo` (replies with the user's text),
// `vision` (reports the media it received), `probe` (writes/reads a file so the
// HITL approval path parks).

import net from 'node:net';
import { spawn, execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

export const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// Build the server once, up front, and resolve its binary path. We spawn the
// binary directly (not `cargo run`) so each server is a single process the
// harness can kill cleanly — a `cargo run` wrapper would leave the real server
// orphaned and keep Node alive past the test.
let serverBin = null;
function ensureBuilt() {
  if (serverBin) return serverBin;
  const out = execSync(
    'cargo build --quiet --message-format=json -p awaken-server-local --bin awaken-server-local',
    { cwd: REPO_ROOT, maxBuffer: 64 * 1024 * 1024 },
  ).toString();
  for (const line of out.split('\n')) {
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.executable && msg.target?.name === 'awaken-server-local') serverBin = msg.executable;
  }
  if (!serverBin) throw new Error('could not resolve the awaken-server-local binary path');
  return serverBin;
}

// A 64x64 solid-red PNG, base64-encoded (deterministic, generated offline). The
// `vision` stub model reports its media type; a real vision model would read it.
export const RED_PNG_B64 =
  'iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAIAAAAlC+aJAAAAb0lEQVR4nO3PAQkAAAyEwO9feoshgnABdLep8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3IPanc8OLDQitxAAAAAElFTkSuQmCC';
export const RED_PNG_DATA_URI = `data:image/png;base64,${RED_PNG_B64}`;

export function waitForPort(port, timeoutMs = 180_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      const sock = net.createConnection({ port, host: '127.0.0.1' });
      sock.once('connect', () => {
        sock.destroy();
        resolve();
      });
      sock.once('error', () => {
        sock.destroy();
        if (Date.now() > deadline) reject(new Error(`server did not listen on ${port}`));
        else setTimeout(attempt, 200);
      });
    };
    attempt();
  });
}

/// Spawn the server in `mode` on `port`, run `fn(baseUrl)`, then stop it.
export async function withServer(mode, port, fn) {
  const bin = ensureBuilt();
  const addr = `127.0.0.1:${port}`;
  const server = spawn(bin, {
    env: { ...process.env, AWAKEN_HTTP_ADDR: addr, AWAKEN_MODEL_MODE: mode },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  let exitedEarly = false;
  server.on('exit', (code) => {
    if (code !== null && code !== 0) exitedEarly = true;
  });
  try {
    await waitForPort(port);
    if (exitedEarly) throw new Error('server exited before it listened');
    return await fn(`http://${addr}`);
  } finally {
    server.kill('SIGINT');
  }
}

const FAKE_KEY = 'sk-fake-upstream-key'; // awaken-allow: secret

// The REAL-provider equivalent of `withServer`: instead of an in-process stub
// model, start a fake Anthropic upstream reproducing `behavior`'s scenario replies,
// point the server's GenaiExecutor at it (`AWAKEN_MODEL_MODE=real`), run `fn`, then
// tear both down. This is how an e2e drops its model stub: the same scenario now
// runs through the real provider adapter + a real socket + the real Anthropic wire.
// `scenario`, when set, selects the server's host-config (client tools / delegate
// roster / skills / memory dir) via `AWAKEN_SCENARIO` for the modes whose config is
// not the plain default. `extraEnv` layers on (e.g. `AWAKEN_MEMORY_DIR`).
export async function withRealServer(behavior, port, fn, { scenario, extraEnv = {} } = {}) {
  const bin = ensureBuilt();
  const addr = `127.0.0.1:${port}`;
  const upstream = await startFakeAnthropic(FAKE_KEY, { behavior });
  const server = spawn(bin, {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: addr,
      AWAKEN_MODEL_MODE: 'real',
      ANTHROPIC_API_KEY: FAKE_KEY,
      ANTHROPIC_BASE_URL: `${upstream.url}/v1/`,
      ANTHROPIC_MODEL: 'fake-haiku',
      ...(scenario ? { AWAKEN_SCENARIO: scenario } : {}),
      ...extraEnv,
    },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  let exitedEarly = false;
  server.on('exit', (code) => {
    if (code !== null && code !== 0) exitedEarly = true;
  });
  try {
    await waitForPort(port);
    if (exitedEarly) throw new Error('server exited before it listened');
    return await fn(`http://${addr}`, upstream);
  } finally {
    server.kill('SIGINT');
    upstream.close();
  }
}

// Spawn the server without a fixed lifetime, so a test can stop and restart it
// (e.g. to verify durable state survives a process restart). `extraEnv` layers on
// top of the inherited environment — pass `AWAKEN_STORAGE_DIR` for durability.
export function spawnServer(mode, port, extraEnv = {}) {
  const bin = ensureBuilt();
  const addr = `127.0.0.1:${port}`;
  const server = spawn(bin, {
    env: { ...process.env, AWAKEN_HTTP_ADDR: addr, AWAKEN_MODEL_MODE: mode, ...extraEnv },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  return { server, baseUrl: `http://${addr}` };
}

// Stop a spawned server and resolve once the process has actually exited, so the
// TCP port is free and the SQLite files are flushed before a restart rebinds.
export function stopServer(server) {
  return new Promise((resolve) => {
    if (server.exitCode !== null) return resolve();
    server.on('exit', () => resolve());
    server.kill('SIGINT');
  });
}

export function pass(msg) {
  console.log(`  ok: ${msg}`);
}
