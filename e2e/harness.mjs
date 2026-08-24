// Shared e2e harness: spawn awaken-server in a chosen model mode, wait for
// it to listen, run a body, and always shut it down. Model modes are the
// deterministic stub models (no API key): `echo` (replies with the user's text),
// `vision` (reports the media it received), `probe` (writes/reads a file so the
// HITL approval path parks).

import net from 'node:net';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { automatedAllInOneArgs } from './awaken_cli_args.mjs';
import {
  AWAKEN_BIN_ENV,
  SCENARIO_HOST_BIN_ENV,
  cargoExecutable,
} from './cargo_binary.mjs';

// The historical 38xxx defaults overlap Linux's ephemeral client-port range.
// Assign each Node scenario a small, non-ephemeral block before the importing
// module reads E2E_PORT. Explicit caller/stage assignments remain authoritative.
const processPortBase = 20_000 + (process.pid % 100) * 100;
process.env.E2E_PORT ??= String(processPortBase);
process.env.E2E_WORKER_PORT ??= String(processPortBase + 50);

export const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
// One test-side owner for the public User Profiles beta vocabulary. Individual
// scenarios still own their cause/effect assertions, but must not fork the wire
// version string from the protocol contract.
export const USER_PROFILES_BETA = 'user-profiles-2026-03-24';
// Canonical secret-free evidence for a test driver that stands in for the
// installed Native provider adapter while it claims (but never executes) a
// production-authored seed dispatch. Keep the wire encoding here so fixtures do
// not grow independently ordered copies of the credential capability contract.
export const WORKER_PROVIDER_CREDENTIAL_CAPABILITY =
  'credential-realization.awaken.dev/v1:{"holders":[{"boundary":"worker","trust_domain":"awaken.worker"}],"material_sources":["control_plane_reference"],"realization_kinds":["worker_provider_adapter"],"recipient_bound_envelopes":false}';
// Match scripts/ci/_cargo_target.sh: a caller-owned coverage/isolated target
// wins; otherwise keep this worktree's Cargo artifacts out of any user-level
// shared target that can contain same-name packages from another worktree.
// Decision table: explicit target => preserve; absent target => repo/target.
process.env.CARGO_TARGET_DIR ??= path.join(REPO_ROOT, 'target');
export const E2E_HOME_ROOT = `/tmp/awaken-e2e-home-${process.pid}`;
export const E2E_HOME = `${E2E_HOME_ROOT}/home`;
cleanupFixtureTree(E2E_HOME_ROOT);
fs.mkdirSync(`${E2E_HOME}/.awaken`, { recursive: true });
fs.writeFileSync(
  `${E2E_HOME}/.awaken/config.toml`,
  `data_dir = ${JSON.stringify(`${E2E_HOME_ROOT}/data`)}\nidentity_mode = "no-login"\nsandbox_tier = "local"\n`,
);
process.on('exit', () => cleanupFixtureTree(E2E_HOME_ROOT));

// A2A publication-pin cause/effect rule: the advertised securitySchemes and
// ordered security requirements are the complete security surface -> hash their
// canonical JSON pair; omitted fields -> the protocol defaults ({}, []). Other
// Agent Card metadata cannot silently change the transport security identity.
export function agentCardSecurityFingerprint(card) {
  return `sha256:${createHash('sha256')
    .update(JSON.stringify([card.securitySchemes ?? {}, card.security ?? []]))
    .digest('hex')}`;
}

// Create the standard, typed deployment input for a scenario that needs its own
// durable control-plane root. Tests must not resurrect the removed AWAKEN_MGMT_*
// configuration path. HOME remains OS metadata; the product reads the one
// authoritative ~/.awaken/config.toml below.
export function deploymentEnv(
  dataDir,
  { identityMode, iamWorkspaces = [], controlSealKey, cloudIam, databases = {}, fields = {} } = {},
) {
  const home = path.join(dataDir, 'e2e-home');
  const configDir = path.join(home, '.awaken');
  fs.mkdirSync(configDir, { recursive: true });
  const lines = [`data_dir = ${JSON.stringify(dataDir)}`];
  if (identityMode) lines.push(`identity_mode = ${JSON.stringify(identityMode)}`);
  if (iamWorkspaces.length > 0) lines.push(`iam_workspaces = ${JSON.stringify(iamWorkspaces)}`);
  if (controlSealKey) lines.push(`control_seal_key = ${JSON.stringify(controlSealKey)}`);
  for (const [field, value] of Object.entries(databases)) {
    if (value) lines.push(`${field} = ${JSON.stringify(value)}`);
  }
  // Generic process fixtures run in an explicitly selected local sandbox. A
  // scenario that exercises another backend overrides this through `fields`.
  for (const [field, value] of Object.entries({ sandbox_tier: 'local', ...fields })) {
    if (value !== undefined) lines.push(`${field} = ${JSON.stringify(value)}`);
  }
  if (cloudIam) {
    lines.push(`cloud_iam_url = ${JSON.stringify(cloudIam.url)}`);
    lines.push(`cloud_iam_issuer = ${JSON.stringify(cloudIam.issuer)}`);
    lines.push(`cloud_iam_audience = ${JSON.stringify(cloudIam.audience)}`);
    lines.push(`cloud_access_token = ${JSON.stringify(cloudIam.accessToken)}`);
    lines.push(`cloud_iam_service_token = ${JSON.stringify(cloudIam.serviceToken)}`);
  }
  fs.writeFileSync(path.join(configDir, 'config.toml'), `${lines.join('\n')}\n`);
  return { HOME: home };
}

// One test-side transport owner for official Managed SDK calls against a
// workspace-scoped Awaken route. Causes: C1=the SDK emits its canonical `/v1/`
// path; C2=the scenario owns the selected Workspace id. Effects: E1=prefix the
// path exactly once before network IO; E2=leave the SDK-owned method, body,
// beta headers, pagination, retries, and response/error decoding unchanged.
// Constraint K1: callers may use raw HTTP only for Awaken-internal, fault, or
// independent wire-oracle routes. Decision rules: M1 C1+C2=>E1+E2; M2 !C1=>
// fail before IO. The four resource scenarios exercise M1; no caller may
// recreate this fetch adapter.
export function managedWorkspaceClient(baseURL, workspace) {
  return new Anthropic({
    apiKey: 'e2e-dummy',
    baseURL,
    maxRetries: 0,
    fetch: async (input, init) => {
      const source = typeof input === 'string' || input instanceof URL ? input : input.url;
      const url = new URL(source);
      if (!url.pathname.startsWith('/v1/')) {
        throw new TypeError(`unexpected official Managed SDK path ${url.pathname}`);
      }
      url.pathname = `/v1/workspaces/${encodeURIComponent(workspace)}${url.pathname.slice(3)}`;
      return fetch(url, init);
    },
  });
}

// Files upload test-wire cause/effect table:
// C1=current Managed Files contract + caller bytes/name -> E1=one multipart
// `file` part; C2=retired purpose/scope metadata -> E2=never emitted here.
// K1=the official SDK/product contract remains the validation authority; this
// helper owns only raw E2E multipart construction, not route, beta, auth, or
// lifecycle policy. Decision rule F1: C1+C2 => E1+E2. The raw Files scenarios
// below exercise F1 through distinct persistence, IAM, reclamation, ephemeral,
// distributed, and Worker-resource effects rather than cloning the wire codec.
export function managedFileUploadForm(content, filename) {
  const form = new FormData();
  form.append('file', new Blob([content]), filename);
  return form;
}

// Build the server once, up front, and resolve its binary path. We spawn the
// binary directly (not `cargo run`) so each server is a single process the
// harness can kill cleanly — a `cargo run` wrapper would leave the real server
// orphaned and keep Node alive past the test.
let serverBin = null;
let productionBin = null;
const spawnedServersByPort = new Map();
const spawnedServerPorts = new WeakMap();

export function trackSpawnedServer(port, server) {
  spawnedServersByPort.set(port, server);
  spawnedServerPorts.set(server, port);
  return server;
}

function untrackSpawnedServer(server) {
  const port = spawnedServerPorts.get(server);
  if (port !== undefined && spawnedServersByPort.get(port) === server) {
    spawnedServersByPort.delete(port);
  }
  spawnedServerPorts.delete(server);
}

// Cause graph: inherited/fixture environment may contain an old listen
// address; the address selected for this process is the sole cause of its bind
// address. Decision table:
//
// inherited address | configured address | selected address || result
// absent            | absent             | A                || A
// stale             | absent             | A                || A
// stale             | stale              | A                || A
// any               | any                | B                || B
//
// Keeping this merge in one place prevents the three spawn entry points from
// recreating competing precedence rules.
function serverProcessEnv(addr, configured = {}) {
  // Ephemeral scenarios previously let every child invent a process-named
  // /tmp sandbox root that the Node owner could not clean after a hard crash.
  // A durable storage root remains authoritative; otherwise the harness-owned
  // tree is the one cleanup boundary for HOME plus sandbox projections.
  const hasDurableRoot = configured.SESSION_DEPLOYMENT_STORAGE_DIR
    ?? process.env.SESSION_DEPLOYMENT_STORAGE_DIR;
  const sandboxDir = configured.SESSION_DEPLOYMENT_SANDBOX_DIR
    ?? process.env.SESSION_DEPLOYMENT_SANDBOX_DIR
    ?? (hasDurableRoot ? undefined : `${E2E_HOME_ROOT}/sandboxes`);
  return {
    ...process.env,
    HOME: E2E_HOME,
    ...configured,
    // A caller's shell may globally clamp Rust logs (Codex commonly uses
    // `warn`). That filter also controls tracing spans, so a trace-capture E2E
    // would otherwise exercise the request successfully while exporting no
    // evidence at all. Trace scenarios own their minimum deterministic filter;
    // an explicit per-scenario RUST_LOG still wins.
    RUST_LOG: configured.RUST_LOG ?? (configured.AWAKEN_TRACE_FILE ? 'info' : process.env.RUST_LOG),
    ...(sandboxDir ? { SESSION_DEPLOYMENT_SANDBOX_DIR: sandboxDir } : {}),
    AWAKEN_HTTP_ADDR: addr,
    AWAKEN_E2E_SHUTDOWN_ON_STDIN_EOF: '1',
  };
}

function ensureBuilt() {
  if (serverBin) return serverBin;
  serverBin = cargoExecutable({
    cwd: REPO_ROOT,
    packageName: 'awaken-scenario-host',
    targetName: 'awaken-scenario-host',
    prebuiltEnvironmentName: SCENARIO_HOST_BIN_ENV,
  });
  return serverBin;
}

export function ensureProductionBuilt() {
  if (productionBin) return productionBin;
  productionBin = cargoExecutable({
    cwd: REPO_ROOT,
    packageName: 'awaken-cli',
    targetName: 'awaken',
    prebuiltEnvironmentName: AWAKEN_BIN_ENV,
  });
  return productionBin;
}

// Start the production composition from its single typed deployment source.
// Scenario-only metadata may be passed as process metadata, but deployment,
// credential, model, and resource configuration must remain in config.toml.
/**
 * @param {string} dataDir
 * @param {number} port
 * @param {{
 *   workspace?: string,
 *   controlSealKey?: string,
 *   databases?: Record<string, string>,
 *   fields?: Record<string, unknown>,
 *   extraEnv?: NodeJS.ProcessEnv,
 *   stderr?: import('node:child_process').StdioOptions[2],
 *   identityMode?: string,
 * }} [options]
 */
export function spawnProduction(
  dataDir,
  port,
  {
    workspace,
    controlSealKey,
    databases = {},
    fields = {},
    extraEnv = {},
    stderr = 'inherit',
    identityMode = 'no-login',
  } = {},
) {
  // Production-composition E2Es that use this generic helper do not exercise
  // ACP. Keep their startup independent of host CLI discovery and network
  // wrapper acquisition; ACP-specific scenarios author their exact selection
  // through their own production config.
  const isolatedFields = { acp_clis: [], ...fields };
  const env = {
    ...process.env,
    ...deploymentEnv(dataDir, {
      identityMode,
      controlSealKey,
      databases,
      fields: isolatedFields,
    }),
    ...extraEnv,
  };
  if (workspace) env.AWAKEN_SCENARIO_WORKSPACE = workspace;
  return trackSpawnedServer(
    port,
    spawn(ensureProductionBuilt(), automatedAllInOneArgs('--port', String(port)), {
      env,
      stdio: ['ignore', 'ignore', stderr],
    }),
  );
}

// A 64x64 solid-red PNG, base64-encoded (deterministic, generated offline). The
// `vision` stub model reports its media type; a real vision model would read it.
export const RED_PNG_B64 =
  'iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAIAAAAlC+aJAAAAb0lEQVR4nO3PAQkAAAyEwO9feoshgnABdLep8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3IPanc8OLDQitxAAAAAElFTkSuQmCC';
export const RED_PNG_DATA_URI = `data:image/png;base64,${RED_PNG_B64}`;

// Canonical bounded wait for committed-state E2Es. The read function owns the
// authoritative projection (HTTP, database, or filesystem); this helper only
// coordinates observation and never drives the product state machine.
// Decision table: predicate true => return the observed value; predicate false
// before deadline => retry; predicate false at deadline => fail with the last
// value; read failure => surface it immediately instead of masking corruption.
export async function waitForValue(
  read,
  predicate,
  description,
  { timeoutMs = 20_000, pollMs = 100 } = {},
) {
  const deadline = performance.now() + timeoutMs;
  let value;
  do {
    value = await read();
    if (await predicate(value)) return value;
    await new Promise((resolve) => setTimeout(resolve, pollMs));
  } while (performance.now() < deadline);
  throw new Error(`${description}; last observed value: ${JSON.stringify(value)}`);
}

// Canonical receipt-scoped observation for asynchronous Managed Event batches.
// Causes: C1=send returned an exact durable receipt id; C2=history may omit it,
// retain it unprocessed, or commit it later; C3=the caller's scenario effect may
// commit after C2; C4=the caller may supply pagination params and either an
// explicit beta list or no beta so a registry SDK owns its default. Effects:
// E1=return the full last history, the exact committed receipt event, and only
// the later delta; E2=never let older history satisfy C3; E3=forward C4 without
// manufacturing a beta field.
// Constraint: this adapter only reads the official SDK history and delegates
// bounded coordination to waitForValue; it owns no terminal predicate and never
// drives the Session/Run lifecycle. Decision rules: W1 !C1=>reject; W2 C1&&!C2
// =>retry; W3 C1+C2&&!C3=>retry; W4 C1+C2+C3=>E1+E2; W5 deadline=>surface the
// last observation through waitForValue; W6 C4=>E3.
export async function waitForSessionEventReceipt(
  client,
  sessionId,
  receiptId,
  betas,
  predicate,
  description,
  options,
) {
  if (typeof receiptId !== 'string' || receiptId.length === 0) {
    throw new TypeError('an exact Managed Event receipt id is required');
  }
  const { listParams = {}, ...waitOptions } = options ?? {};
  const sdkListParams = betas === undefined
    ? { ...listParams }
    : { ...listParams, betas };
  return waitForValue(
    async () => {
      const events = [];
      for await (const event of client.beta.sessions.events.list(sessionId, sdkListParams)) {
        events.push(event);
      }
      const receiptIndex = events.findIndex((event) => event.id === receiptId);
      const receiptEvent = receiptIndex < 0 ? undefined : events[receiptIndex];
      return {
        events,
        receiptEvent,
        delta: receiptEvent?.processed_at ? events.slice(receiptIndex + 1) : [],
      };
    },
    async (observation) => Boolean(observation.receiptEvent?.processed_at)
      && await predicate(observation),
    description,
    waitOptions,
  );
}

export function childDirectories(parent) {
  return fs.existsSync(parent)
    ? fs.readdirSync(parent, { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .map((entry) => path.join(parent, entry.name))
      .sort()
    : [];
}

export function onlyChildDirectory(parent, description = 'one owned directory exists') {
  // Opaque-directory decision table: exactly one provider-owned child => return
  // its concrete path; zero/multiple children => fail because realization is
  // absent/ambiguous. Callers must never reimplement the provider's private
  // wire-id-to-filesystem-name mapping.
  const directories = childDirectories(parent);
  if (directories.length !== 1) {
    throw new Error(`${description}: ${JSON.stringify(directories)}`);
  }
  return directories[0];
}

function mountPointsUnder(root) {
  if (process.platform !== 'linux') return [];
  let mountInfo;
  try {
    mountInfo = fs.readFileSync('/proc/self/mountinfo', 'utf8');
  } catch {
    return [];
  }
  const absoluteRoot = path.resolve(root);
  const prefix = `${absoluteRoot}${path.sep}`;
  return mountInfo.split('\n').flatMap((line) => {
    const encoded = line.split(' ')[4];
    if (!encoded) return [];
    const mountPoint = encoded
      .replaceAll('\\040', ' ')
      .replaceAll('\\011', '\t')
      .replaceAll('\\012', '\n')
      .replaceAll('\\134', '\\');
    return mountPoint === absoluteRoot || mountPoint.startsWith(prefix) ? [mountPoint] : [];
  }).sort((left, right) => right.length - left.length);
}

// Canonical cleanup for crash/recovery fixtures that may own FUSE projections.
// Decision table: no mount => ordinary recursive removal; live/disconnected
// mount => detach deepest-first, then remove; detach/removal failure => surface
// the cleanup failure. This is fixture hygiene only and never substitutes for a
// success-path assertion that product teardown removed its owned sandbox.
export function cleanupFixtureTree(root) {
  for (const mountPoint of mountPointsUnder(root)) {
    let detached = false;
    for (const command of ['fusermount3', 'fusermount']) {
      const result = spawnSync(command, ['-uz', mountPoint], { stdio: 'ignore' });
      if (result.status === 0) {
        detached = true;
        break;
      }
    }
    if (!detached) throw new Error(`could not detach fixture mount ${mountPoint}`);
  }
  fs.rmSync(root, { recursive: true, force: true });
}

/**
 * @param {number} port
 * @param {number} [timeoutMs]
 * @param {import('node:child_process').ChildProcess | null} [server]
 * @returns {Promise<void>}
 */
export function waitForPort(port, timeoutMs = 900_000, server = null) {
  // Readiness is an elapsed-time deadline. Wall-clock adjustments can jump
  // `Date.now()` past the deadline between retries even though the child has
  // just announced that it is listening; the monotonic clock cannot.
  const deadline = performance.now() + timeoutMs;
  // R1 explicit child => observe it; R2 omitted child for a harness-spawned
  // server => recover the canonical port/child registration; R3 external port
  // => retain deadline-only probing. A child exit is terminal in R1/R2.
  const observedServer = server ?? spawnedServersByPort.get(port) ?? null;
  return new Promise((resolve, reject) => {
    let settled = false;
    let retry;
    const cleanup = () => {
      if (retry) clearTimeout(retry);
      observedServer?.off('exit', onExit);
      observedServer?.off('close', onClose);
      observedServer?.off('error', onError);
    };
    const finish = (error) => {
      if (settled) return;
      settled = true;
      cleanup();
      if (error) reject(error);
      else resolve();
    };
    const terminalError = (detail) => new Error(
      `server exited before it listened on ${port} (${detail})`,
    );
    function onExit(code, signal) {
      finish(terminalError(`code=${code}, signal=${signal}`));
    }
    function onClose(code, signal) {
      finish(terminalError(`closed: code=${code}, signal=${signal}`));
    }
    function onError(error) {
      finish(terminalError(`spawn=${error?.code ?? error}`));
    }
    observedServer?.once('exit', onExit);
    observedServer?.once('close', onClose);
    observedServer?.once('error', onError);
    const attempt = () => {
      if (settled) return;
      const sock = net.createConnection({ port, host: '127.0.0.1' });
      sock.once('connect', () => {
        sock.destroy();
        finish();
      });
      sock.once('error', () => {
        sock.destroy();
        if (
          observedServer &&
          (observedServer.exitCode !== null || observedServer.signalCode !== null)
        ) {
          finish(terminalError(
            `code=${observedServer.exitCode}, signal=${observedServer.signalCode}`,
          ));
        } else if (performance.now() > deadline) finish(new Error(`server did not listen on ${port}`));
        else retry = setTimeout(attempt, 200);
      });
    };
    attempt();
  });
}

function waitForServer(server, port) {
  return waitForPort(port, 900_000, server);
}

export async function availablePort(preferred) {
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
    // Generic scenario fixtures use the portable Workdir substrate. A test for
    // Namespace/Container owns its provider selection explicitly and does not
    // use this convenience wrapper.
    env: serverProcessEnv(addr, {
      AWAKEN_MODEL_MODE: mode,
      SESSION_DEPLOYMENT_SANDBOX_TIER: 'local',
    }),
    stdio: ['pipe', 'inherit', 'inherit'],
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
export function startUpstream(behavior, opts = {}) {
  return startFakeAnthropic(FAKE_KEY, { behavior, ...opts });
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
// MODEL swaps to the real wire. `opts.extraEnv` layers on scenario-specific settings.
export function realServerEnv(behavior, upstream, { mode = 'real', extraEnv = {} } = {}) {
  return {
    AWAKEN_MODEL_MODE: mode,
    ...(mode === 'real' ? {} : { AWAKEN_MODEL_SOURCE: 'http' }),
    // scenario-host consumes its typed Deployment from explicit fixture input;
    // unlike the production CLI it does not rediscover sandbox_tier from
    // config.toml. Provider-backed fixtures therefore select their portable
    // Local substrate here. A substrate-specific test can still override this
    // through extraEnv and must fail closed when that substrate is unavailable.
    SESSION_DEPLOYMENT_SANDBOX_TIER: 'local',
    ANTHROPIC_API_KEY: FAKE_KEY,
    ANTHROPIC_BASE_URL: `${upstream.url}/v1/`,
    ANTHROPIC_MODEL: 'fake-haiku',
    ...extraEnv,
  };
}

export async function withRealServer(behavior, port, fn, opts = {}) {
  const bin = ensureBuilt();
  // One authoritative fake-provider fixture owns both inference behavior and
  // optional model discovery; callers must not start a parallel `/v1/models`
  // server merely to seed the live catalog.
  const upstream = await startFakeAnthropic(FAKE_KEY, {
    behavior,
    ...(opts.upstream ?? {}),
  });
  const listenPort = await availablePort(port);
  const addr = `127.0.0.1:${listenPort}`;
  // When `opts.capture` is set, pipe the child's stdout/stderr so a test can scan
  // the server logs (e.g. the secret-non-leak invariant), teeing them through to
  // this process's streams so behavior is unchanged for a human watching. The
  // accumulated text is exposed to `fn` as a third `{ text() }` argument.
  const capture = opts.capture ? { buf: '' } : null;
  const server = spawn(bin, {
    env: serverProcessEnv(addr, realServerEnv(behavior, upstream, opts)),
    stdio: capture ? ['pipe', 'pipe', 'pipe'] : ['pipe', 'inherit', 'inherit'],
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
// top of the inherited environment — pass `SESSION_DEPLOYMENT_STORAGE_DIR` for durability.
export function spawnServer(mode, port, extraEnv = {}) {
  const bin = ensureBuilt();
  const addr = `127.0.0.1:${port}`;
  const server = trackSpawnedServer(
    port,
    spawn(bin, {
      env: serverProcessEnv(addr, { ...extraEnv, AWAKEN_MODEL_MODE: mode }),
      stdio: ['pipe', 'inherit', 'inherit'],
    }),
  );
  return { server, baseUrl: `http://${addr}` };
}

// Stop a spawned server and resolve once the process has actually exited, so the
// TCP port is free and the SQLite files are flushed before a restart rebinds.
/**
 * @param {import('node:child_process').ChildProcess} server
 * @returns {Promise<void>}
 */
export function stopServer(server) {
  return new Promise((resolve) => {
    // A deliberately crashed child has `exitCode === null` and a non-null
    // `signalCode`. Treat either terminal form as already stopped; otherwise a
    // recovery e2e that SIGKILLs the worker would subscribe after `exit` fired
    // and wait forever.
    if (server.exitCode !== null || server.signalCode !== null) {
      untrackSpawnedServer(server);
      return resolve();
    }
    let settled = false;
    let forceTimer;
    let fallbackTimer;
    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(forceTimer);
      clearTimeout(fallbackTimer);
      server.off('exit', finish);
      server.off('close', finish);
      server.off('error', finish);
      untrackSpawnedServer(server);
      resolve();
    };
    server.once('exit', finish);
    server.once('close', finish);
    server.once('error', finish);
    // Shutdown decision table:
    // | stdin pipe | process terminal | action |
    // |---|---|---|
    // | open | no | close stdin; server drains and coverage flushes |
    // | absent | no | signal fallback for non-harness children |
    // | any | yes | resolve without a second stop |
    // Windows maps Node SIGINT to forced termination, so EOF is the portable
    // graceful cause; Unix production signal behavior remains independently wired.
    if (server.stdin && !server.stdin.destroyed) server.stdin.end();
    else server.kill('SIGINT');
    // Give the product its full 20s Worker drain budget plus margin. A fixture
    // that still cannot drain is terminated by exact child handle so it cannot
    // stall every later rule in the suite.
    forceTimer = setTimeout(() => {
      server.kill('SIGKILL');
      fallbackTimer = setTimeout(finish, 5_000);
    }, 30_000);
  });
}

export function pass(msg) {
  console.log(`  ok: ${msg}`);
}

// Canonical Managed turn terminal predicate. An `agent.message` may carry an
// intermediate tool-calling assistant fact, so only the committed idle fact with
// `end_turn` proves a multi-step run has finished.
export function hasEndTurn(events) {
  const latestIdle = [...events].reverse().find((event) => event.type === 'session.status_idle');
  return latestIdle?.stop_reason?.type === 'end_turn';
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
