// Shared e2e harness: spawn awaken-server in a chosen model mode, wait for
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
    'cargo build --quiet --message-format=json -p awaken-scenario-host --bin awaken-scenario-host',
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
    if (msg.executable && msg.target?.name === 'awaken-scenario-host') serverBin = msg.executable;
  }
  if (!serverBin) throw new Error('could not resolve the awaken-server binary path');
  return serverBin;
}

// A 64x64 solid-red PNG, base64-encoded (deterministic, generated offline). The
// `vision` stub model reports its media type; a real vision model would read it.
export const RED_PNG_B64 =
  'iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAIAAAAlC+aJAAAAb0lEQVR4nO3PAQkAAAyEwO9feoshgnABdLep8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3IPanc8OLDQitxAAAAAElFTkSuQmCC';
export const RED_PNG_DATA_URI = `data:image/png;base64,${RED_PNG_B64}`;

export function waitForPort(port, timeoutMs = 180_000, server = null) {
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
        if (server && (server.exitCode !== null || server.signalCode !== null)) {
          reject(
            new Error(
              `server exited before it listened on ${port} ` +
                `(code=${server.exitCode}, signal=${server.signalCode})`,
            ),
          );
        } else if (Date.now() > deadline) reject(new Error(`server did not listen on ${port}`));
        else setTimeout(attempt, 200);
      });
    };
    attempt();
  });
}

function waitForServer(server, port) {
  return waitForPort(port, 180_000, server);
}

async function availablePort(preferred) {
  const tryListen = (port) =>
    new Promise((resolve, reject) => {
      const reservation = net.createServer();
      reservation.once('error', reject);
      reservation.listen(port, '127.0.0.1', () => {
        const address = reservation.address();
        const selected = typeof address === 'object' && address ? address.port : port;
        reservation.close(() => resolve(selected));
      });
    });
  try {
    return await tryListen(preferred);
  } catch (error) {
    if (error?.code !== 'EADDRINUSE') throw error;
    return tryListen(0);
  }
}

/// Spawn the server in `mode` on `port`, run `fn(baseUrl)`, then stop it.
export async function withServer(mode, port, fn) {
  const bin = ensureBuilt();
  const listenPort = await availablePort(port);
  const addr = `127.0.0.1:${listenPort}`;
  const server = spawn(bin, {
    env: { ...process.env, AWAKEN_HTTP_ADDR: addr, AWAKEN_MODEL_MODE: mode },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  try {
    await waitForServer(server, listenPort);
    return await fn(`http://${addr}`);
  } finally {
    await stopServer(server);
  }
}

// The fake-upstream key the real-wire harness authenticates with, exported so a
// test that spawns the server itself (restart/durability suites) can build the same
// env via `realServerEnv`.
export const FAKE_KEY = 'sk-fake-upstream-key'; // awaken-allow: secret

// A scenario with host config (custom tools / delegates / skills / state machine /
// compaction / memory / config plane / MCP): keep its `mode` router but run the
// model for real (`behavior` on the wire). `mode` and `behavior` are named
// separately because they often differ (mode `delegate` ↔ behavior `delegating`,
// mode `management` ↔ behavior `mcp`, mode `git-repo` ↔ behavior `gitRepo`).
export function withScenarioServer(mode, behavior, port, fn, extraEnv = {}, opts = {}) {
  return withRealServer(behavior, port, fn, { mode, extraEnv, ...opts });
}

// A standalone fake Anthropic upstream reproducing `behavior`, for tests that
// spawn/restart the server themselves (durability/restart suites): the upstream is
// created ONCE and survives every restart, so each spawned process dials the same
// URL. Pair with `realServerEnv(behavior, upstream, {mode})` in the spawn env, and
// `upstream.close()` in a `finally`.
export function startUpstream(behavior) {
  return startFakeAnthropic(FAKE_KEY, { behavior });
}

// The REAL-provider equivalent of `withServer`: instead of an in-process stub
// model, start a fake Anthropic upstream reproducing `behavior`'s scenario replies,
// point the server's GenaiExecutor at it, run `fn`, then tear both down. This is how
// an e2e drops its model stub: the same scenario runs through the real provider
// adapter + a real socket + the real Anthropic wire.
//
// Two axes (see the server's `scenario_model`): the MODEL is always real here; the
// server's HOST CONFIG is chosen by `opts.mode`. A plain-mount scenario (echo /
// vision / probe / …) needs no host config, so the default `mode: 'real'` boots the
// bare real-model router. A scenario with host config (custom tools, delegate
// roster, skills, state machine, compaction, memory, config plane) keeps its
// `AWAKEN_MODEL_MODE=<mode>` router and sets `AWAKEN_MODEL_SOURCE=http` so only its
// MODEL swaps to the real wire. `opts.extraEnv` layers on (e.g. `AWAKEN_MEMORY_DIR`).
export function realServerEnv(behavior, upstream, { mode = 'real', extraEnv = {} } = {}) {
  return {
    AWAKEN_MODEL_MODE: mode,
    ...(mode === 'real' ? {} : { AWAKEN_MODEL_SOURCE: 'http' }),
    ANTHROPIC_API_KEY: FAKE_KEY,
    ANTHROPIC_BASE_URL: `${upstream.url}/v1/`,
    ANTHROPIC_MODEL: 'fake-haiku',
    ...extraEnv,
  };
}

export async function withRealServer(behavior, port, fn, opts = {}) {
  const bin = ensureBuilt();
  const upstream = await startFakeAnthropic(FAKE_KEY, { behavior });
  const listenPort = await availablePort(port);
  const addr = `127.0.0.1:${listenPort}`;
  // When `opts.capture` is set, pipe the child's stdout/stderr so a test can scan
  // the server logs (e.g. the secret-non-leak invariant), teeing them through to
  // this process's streams so behavior is unchanged for a human watching. The
  // accumulated text is exposed to `fn` as a third `{ text() }` argument.
  const capture = opts.capture ? { buf: '' } : null;
  const server = spawn(bin, {
    env: { ...process.env, AWAKEN_HTTP_ADDR: addr, ...realServerEnv(behavior, upstream, opts) },
    stdio: capture ? ['ignore', 'pipe', 'pipe'] : ['ignore', 'inherit', 'inherit'],
  });
  if (capture) {
    const tee = (chunk, sink) => {
      const s = chunk.toString();
      capture.buf += s;
      sink.write(s);
    };
    server.stdout.on('data', (c) => tee(c, process.stdout));
    server.stderr.on('data', (c) => tee(c, process.stderr));
  }
  try {
    await waitForServer(server, listenPort);
    return await fn(`http://${addr}`, upstream, capture ? { text: () => capture.buf } : null);
  } finally {
    await stopServer(server);
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
    // A deliberately crashed child has `exitCode === null` and a non-null
    // `signalCode`. Treat either terminal form as already stopped; otherwise a
    // recovery e2e that SIGKILLs the worker would subscribe after `exit` fired
    // and wait forever.
    if (server.exitCode !== null || server.signalCode !== null) return resolve();
    server.on('exit', () => resolve());
    server.kill('SIGINT');
  });
}

export function pass(msg) {
  console.log(`  ok: ${msg}`);
}

// Reassemble the assistant's text from a streamed SSE body the way a real client
// (`useChat`, `HttpAgent`) does: concatenate the `delta` field of every `data:`
// frame that carries one. Both wire shapes chunk the reply across many frames —
// AI SDK `text-delta` and AG-UI `TEXT_MESSAGE_CONTENT` both use `delta` — so a
// raw `body.includes("Echo: X")` never sees the contiguous phrase. This yields
// the joined text to assert against instead.
export function streamedText(body) {
  let out = '';
  for (const line of body.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed.startsWith('data:')) continue;
    const payload = trimmed.slice(5).trim();
    if (payload === '[DONE]') continue;
    let frame;
    try {
      frame = JSON.parse(payload);
    } catch {
      continue; // non-JSON keep-alive / comment lines
    }
    if (typeof frame.delta === 'string') out += frame.delta;
  }
  return out;
}
