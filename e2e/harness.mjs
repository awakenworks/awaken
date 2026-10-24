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
import { tmpdir } from 'node:os';
import Anthropic from '@anthropic-ai/sdk';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { automatedAllInOneArgs } from './awaken_cli_args.mjs';
import {
  AWAKEN_BIN_ENV,
  SCENARIO_HOST_BIN_ENV,
  cargoExecutable,
  cargoExecutionBundle,
  cargoScenarioHostBundle,
} from './cargo_binary.mjs';
import { sqliteRows, stringifyDurableJson } from './sqlite.mjs';
import { withoutAmbientProviderKeys } from './process_environment.mjs';

// The historical 38xxx defaults overlap Linux's ephemeral client-port range.
// Assign each Node scenario a small, non-ephemeral block before the importing
// module reads E2E_PORT. Explicit caller/stage assignments remain authoritative.
const processPortBase = process.env.E2E_PORT && process.env.E2E_WORKER_PORT
  ? undefined
  : await availablePortBlock(20_000 + (process.pid % 100) * 100);
process.env.E2E_PORT ??= String(processPortBase);
process.env.E2E_WORKER_PORT ??= String(processPortBase + 50);

export const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
// One test-side owner for the public User Profiles beta vocabulary. Individual
// scenarios still own their cause/effect assertions, but must not fork the wire
// version string from the protocol contract.
export const USER_PROFILES_BETA = 'user-profiles-2026-03-24';
// One test-side owner for the Skills beta vocabulary. SDK 0.122 makes this
// caller-supplied on every `beta.skills` operation, so a copied literal can
// silently select the GA route or fail beta admission after an SDK upgrade.
export const SKILLS_BETA = 'skills-2025-10-02';
export const SKILLS_BETAS = [SKILLS_BETA];
// One test-side owner for the Files beta vocabulary. SDK 0.122 requires the
// caller to opt in on every `beta.files` operation. Causes: an SDK upgrade or
// a copied/stale selector; effects: beta admission succeeds or fails before the
// File aggregate is reached. Decision rule: every beta Files caller composes
// this selector; GA Files callers deliberately omit it and test that route.
export const FILES_BETA = 'files-api-2025-04-14';
export const FILES_BETAS = [FILES_BETA];
// The Memory/full-chain scenario publishes exactly one real Resource definition
// before mounting its immutable Agent publication. Cause/effect rule: C1 the
// deterministic scenario exposes one terminal list page with exactly one Store
// -> E1 return that server-owned identity; C2 malformed, empty, additional, or
// paginated data -> E2 fail rather than guessing a first/default Memory.
export async function scenarioMemoryStore(client, headers) {
  const page = await client.get('/v1/memory_stores', { headers });
  const store = page?.data?.[0];
  if (
    !Array.isArray(page?.data)
    || page.data.length !== 1
    || page.next_page !== null
    || !store
    || typeof store !== 'object'
    || typeof store.id !== 'string'
    || store.id.length === 0
  ) {
    throw new Error(
      'deterministic Memory scenario must publish exactly one MemoryStore '
      + `on one terminal page; observed ${JSON.stringify(page)}`,
    );
  }
  return store;
}
// Match scripts/ci/_cargo_target.sh: a caller-owned coverage/isolated target
// wins; otherwise keep this worktree's Cargo artifacts out of any user-level
// shared target that can contain same-name packages from another worktree.
// Decision table: explicit target => preserve; absent target => repo/target.
process.env.CARGO_TARGET_DIR ??= path.join(REPO_ROOT, 'target');
// Each module instance owns a freshly allocated path below the caller's
// native temp root. Never delete a predictable pre-existing path on import.
export const E2E_HOME_ROOT = fs.mkdtempSync(path.join(tmpdir(), 'awaken-e2e-home-'));
export const E2E_HOME = path.join(E2E_HOME_ROOT, 'home');
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
  if (typeof identityMode !== 'string' || identityMode.length === 0) {
    throw new TypeError('deploymentEnv requires an explicit identityMode');
  }
  const home = path.join(dataDir, 'e2e-home');
  const configDir = path.join(home, '.awaken');
  fs.mkdirSync(configDir, { recursive: true });
  const lines = [
    `data_dir = ${JSON.stringify(dataDir)}`,
    `identity_mode = ${JSON.stringify(identityMode)}`,
  ];
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

// Direct-SQL E2E probes must address the same database as the product under
// test. Cause/effect rule: one absolute PostgreSQL URL with one path segment
// yields that decoded database name; an empty/nested/non-PostgreSQL URL is
// rejected before a probe can accidentally inspect the daemon's default DB.
export function postgresDatabaseName(databaseUrl) {
  const url = new URL(databaseUrl);
  const encodedName = url.pathname.slice(1);
  if (
    !['postgres:', 'postgresql:'].includes(url.protocol)
    || encodedName.length === 0
    || encodedName.includes('/')
  ) {
    throw new Error(`invalid PostgreSQL database URL for E2E probe: ${databaseUrl}`);
  }
  return decodeURIComponent(encodedName);
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
const initializedInstallationConfigurations = new Set();
const spawnedServersByPort = new Map();
const spawnedServerStates = new WeakMap();

export function trackSpawnedServer(port, server, readinessLine = null) {
  const state = { port, ready: readinessLine === null, onData: null };
  if (readinessLine !== null) {
    if (!server.stderr) throw new Error('Scenario readiness requires the child stderr pipe');
    let pending = '';
    state.onData = (chunk) => {
      pending += chunk.toString();
      const lines = pending.split('\n');
      pending = lines.pop();
      if (lines.some(line => line.replace(/\r$/u, '') === readinessLine)) state.ready = true;
    };
    server.stderr.on('data', state.onData);
  }
  spawnedServersByPort.set(port, server);
  spawnedServerStates.set(server, state);
  return server;
}

function untrackSpawnedServer(server) {
  const state = spawnedServerStates.get(server);
  if (state && spawnedServersByPort.get(state.port) === server) {
    spawnedServersByPort.delete(state.port);
  }
  if (state?.onData) server.stderr.off('data', state.onData);
  spawnedServerStates.delete(server);
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
function serverProcessEnv(addr, configured = {}, inheritedEnvironment = process.env) {
  // Ephemeral scenarios previously let every child invent a process-named
  // /tmp sandbox root that the Node owner could not clean after a hard crash.
  // A durable storage root remains authoritative; otherwise the harness-owned
  // tree is the one cleanup boundary for HOME plus sandbox projections.
  const hasDurableRoot = configured.SESSION_DEPLOYMENT_STORAGE_DIR
    ?? inheritedEnvironment.SESSION_DEPLOYMENT_STORAGE_DIR;
  const sandboxDir = configured.SESSION_DEPLOYMENT_SANDBOX_DIR
    ?? inheritedEnvironment.SESSION_DEPLOYMENT_SANDBOX_DIR
    ?? (hasDurableRoot ? undefined : `${E2E_HOME_ROOT}/sandboxes`);
  return {
    ...withoutAmbientProviderKeys(inheritedEnvironment),
    HOME: E2E_HOME,
    ...configured,
    // A caller's shell may globally clamp Rust logs (Codex commonly uses
    // `warn`). That filter also controls tracing spans, so a trace-capture E2E
    // would otherwise exercise the request successfully while exporting no
    // evidence at all. Trace scenarios own their minimum deterministic filter;
    // an explicit per-scenario RUST_LOG still wins.
    RUST_LOG: configured.RUST_LOG
      ?? (configured.AWAKEN_TRACE_FILE ? 'info' : inheritedEnvironment.RUST_LOG),
    ...(sandboxDir ? { SESSION_DEPLOYMENT_SANDBOX_DIR: sandboxDir } : {}),
    AWAKEN_HTTP_ADDR: addr,
    AWAKEN_E2E_SHUTDOWN_ON_STDIN_EOF: '1',
  };
}

function ensureBuilt() {
  if (serverBin) return serverBin;
  // C1 direct E2E build; C2 deterministic prebuilt snapshot. E1 C1 builds the
  // existing hand-capable sibling beside scenario-host; E2 C2 requires that
  // immutable sibling but leaves capability validation to runtime-host.
  // Rules: H1 C1=>E1; H2 C2=>E2. One bundle helper prevents fixtures from
  // independently guessing Cargo features or companion paths.
  ({ scenarioHost: serverBin } = cargoScenarioHostBundle({
    cwd: REPO_ROOT,
    environment: process.env,
    prebuiltEnvironmentName: SCENARIO_HOST_BIN_ENV,
  }));
  return serverBin;
}

export function ensureProductionBuilt() {
  if (productionBin) return productionBin;
  // Cold production fixtures require the same hand companion as release bundles.
  ({ executable: productionBin } = cargoExecutionBundle({
    cwd: REPO_ROOT,
    packageName: 'awaken-cli',
    targetName: 'awaken',
    prebuiltEnvironmentName: AWAKEN_BIN_ENV,
  }));
  return productionBin;
}

// The production installation state machine remains the only authority for
// first-use storage. This E2E adapter owns only when a positive fixture invokes
// that existing CLI boundary. Cause/effect decision table:
//
// | Rule | exact config observation | prior successful observation | effect |
// |---|---|---|---|
// | I1 | valid | no | run explicit initialization before child spawn |
// | I2 | unchanged | yes | skip migration so restart uses ordinary Serve |
// | I3 | initialization fails | no | throw, do not cache, do not spawn child |
// | I4 | unchanged but durable bytes later corrupt | yes | skip migration; Serve detects damage |
// | I5 | path, cwd, config bytes, or resolved binary identity changes | either | independently initialize/verify |
//
// Constraint K1: this helper never adopts legacy storage and never weakens the
// read-only Serve fence. Negative startup fixtures deliberately bypass it.
export function initializeE2EInstallation(environment, options = {}) {
  if (!environment || typeof environment !== 'object') {
    throw new TypeError('initializeE2EInstallation requires the child environment');
  }
  if (
    options.configPath === undefined
    && (typeof environment.HOME !== 'string' || environment.HOME.length === 0)
  ) {
    throw new TypeError('initializeE2EInstallation requires the child HOME');
  }
  const cwd = path.resolve(options.cwd ?? process.cwd());
  const configPath = path.resolve(
    cwd,
    options.configPath ?? path.join(environment.HOME, '.awaken', 'config.toml'),
  );
  const binary = options.binary ?? ensureProductionBuilt();
  // Cache only an observation made by the exact executable we later spawn.
  // realpath removes alias spellings; the bytes digest detects an in-place
  // rebuild without inventing a second migration or installation authority.
  const resolvedBinary = fs.realpathSync(path.resolve(cwd, binary));
  const binaryDigest = createHash('sha256')
    .update(fs.readFileSync(resolvedBinary))
    .digest('hex');
  const configBytes = fs.readFileSync(configPath);
  const configDigest = createHash('sha256').update(configBytes).digest('hex');
  const cacheKey = `${cwd}\0${configPath}\0${configDigest}\0${resolvedBinary}\0${binaryDigest}`;
  if (initializedInstallationConfigurations.has(cacheKey)) return 'cached';
  const reference = createHash('sha256')
    .update(`${cwd}\0${configPath}`)
    .digest('hex')
    .slice(0, 24);

  const result = spawnSync(
    resolvedBinary,
    [
      'database',
      'migrate',
      '--config',
      configPath,
      '--initialize-installation',
      '--initialization-reference',
      `e2e-harness-${reference}`,
    ],
    {
      cwd,
      env: environment,
      encoding: 'utf8',
    },
  );
  if (result.error || result.status !== 0) {
    const detail = [result.error?.message, result.stdout, result.stderr]
      .filter(Boolean)
      .join('\n')
      .slice(-8_000);
    throw new Error(
      `explicit E2E installation failed for ${configPath} `
      + `(status=${result.status}, signal=${result.signal}): ${detail}`,
    );
  }
  const finalDigest = createHash('sha256').update(fs.readFileSync(configPath)).digest('hex');
  if (finalDigest !== configDigest) {
    throw new Error(`E2E installation config changed during migration: ${configPath}`);
  }
  initializedInstallationConfigurations.add(cacheKey);
  return 'initialized';
}

function preparedServerProcessEnv(addr, configured = {}, inheritedEnvironment = process.env) {
  const environment = serverProcessEnv(addr, configured, inheritedEnvironment);
  const mode = environment.AWAKEN_MODEL_MODE;
  if (mode === 'management' || mode?.startsWith('management-')) {
    initializeE2EInstallation(environment);
  }
  return environment;
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
 *   stdout?: import('node:child_process').StdioOptions[1],
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
    stdout = 'ignore',
    identityMode = 'no-login',
  } = {},
  inheritedEnvironment = process.env,
) {
  // Production-composition E2Es that use this generic helper do not exercise
  // ACP. Keep their startup independent of host CLI discovery and network
  // wrapper acquisition; ACP-specific scenarios author their exact selection
  // through their own production config.
  const isolatedFields = { acp_clis: [], ...fields };
  const env = {
    ...inheritedEnvironment,
    ...deploymentEnv(dataDir, {
      identityMode,
      controlSealKey,
      databases,
      fields: isolatedFields,
    }),
    ...extraEnv,
  };
  if (workspace) env.AWAKEN_SCENARIO_WORKSPACE = workspace;
  initializeE2EInstallation(env);
  return trackSpawnedServer(
    port,
    spawn(ensureProductionBuilt(), automatedAllInOneArgs('--port', String(port)), {
      env,
      stdio: ['ignore', stdout, stderr],
    }),
  );
}

// One 256x256 solid-red RGB PNG, generated offline with standard PNG chunks,
// filter-0 scanlines and zlib level 6. All multimodal callers share these bytes;
// the live model needs no external image host or aggressively tiny encoding.
export const RED_PNG_B64 =
  'iVBORw0KGgoAAAANSUhEUgAAAQAAAAEACAIAAADTED8xAAADGklEQVR4nO3OQQ0AMBAEofVvupUxjyNBAHsbnNUPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPINQPIPQBWUwO8hhpVuIAAAAASUVORK5CYII=';
export const RED_PNG_DATA_URI = `data:image/png;base64,${RED_PNG_B64}`;

// One classification authority for failures observed while repeating a
// side-effect-free read. HTTP status and SDK transport names are returned for
// diagnostics; null means the caller must fail immediately. This classifier
// must never be attached to an unkeyed write: retry orchestration remains in
// waitForValue, whose caller owns proof that its read is replay-safe.
export function retryableReadStatus(error) {
  const status = error?.status;
  if (typeof status === 'number') {
    return [408, 409, 429, 500, 502, 503, 504].includes(status) ? status : null;
  }
  const name = error?.name;
  if (name === 'APIConnectionError' || name === 'APIConnectionTimeoutError') {
    return String(name);
  }
  // The SDK wraps connection failures during request setup, but a dropped
  // response body can surface directly from undici's async stream iterator.
  // Only the exact transport code is safe to classify as a transient read.
  return name === 'TypeError' && error?.cause?.code === 'UND_ERR_SOCKET'
    ? 'UND_ERR_SOCKET'
    : null;
}

// Canonical bounded wait for committed-state E2Es. The read function owns the
// authoritative projection (HTTP, database, or filesystem); this helper only
// coordinates observation and never drives the product state machine.
// Decision table: predicate true => return the observed value; predicate false
// before deadline => retry; predicate false at deadline => fail with the last
// value; unclassified read failure => surface it immediately instead of masking
// corruption; caller-classified transient read failure => retry the observation
// only, never the product command that produced the state being observed;
// classified failures continuing through the deadline => surface the last exact
// error rather than manufacture an undefined-value predicate timeout.
export async function waitForValue(
  read,
  predicate,
  description,
  { timeoutMs = 20_000, pollMs = 100, retryReadError } = {},
) {
  const deadline = performance.now() + timeoutMs;
  let value;
  let lastRetryableReadError;
  do {
    try {
      value = await read();
      lastRetryableReadError = undefined;
    } catch (error) {
      if (retryReadError?.(error) !== true || performance.now() >= deadline) throw error;
      lastRetryableReadError = error;
      await new Promise((resolve) => setTimeout(resolve, pollMs));
      continue;
    }
    if (await predicate(value)) return value;
    await new Promise((resolve) => setTimeout(resolve, pollMs));
  } while (performance.now() < deadline);
  // If every observation since the last successful read failed transiently,
  // the last exact transport/product error is the useful terminal evidence.
  // A generic predicate timeout remains reserved for a successfully read value
  // that never reached the requested state.
  if (lastRetryableReadError !== undefined) throw lastRetryableReadError;
  // Cause/effect rule: durable SQLite observations may contain exact u64
  // BigInts; timeout diagnostics must preserve those tokens instead of masking
  // the product state with JSON.stringify's BigInt TypeError. Reuse the one
  // durable JSON encoder shared by fault-injection fixtures.
  throw new Error(`${description}; last observed value: ${stringifyDurableJson(value)}`);
}

// One official-SDK owner for complete Managed Session history. Callers that
// need a durable audit snapshot reuse this pagination loop instead of assuming
// one SDK page is the whole history. The server's bounded maximum is 500;
// requesting it here avoids rebuilding the same committed projection once per
// 50-event default page, while preserving explicit caller pagination choices.
export async function listManagedSessionEvents(client, sessionId, betas, listParams = {}) {
  const params = { ...listParams };
  params.limit ??= 500;
  if (betas !== undefined) params.betas = betas;
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, params)) {
    events.push(event);
  }
  return events;
}

function receiptScopedObservation(events, receiptId) {
  const receiptIndex = events.findIndex((event) => event.id === receiptId);
  const receiptEvent = receiptIndex < 0 ? undefined : events[receiptIndex];
  return {
    events,
    receiptEvent,
    delta: receiptEvent?.processed_at ? events.slice(receiptIndex + 1) : [],
  };
}

function mergeManagedSessionEvent(events, positions, event) {
  if (typeof event?.id !== 'string') return;
  const position = positions.get(event.id);
  if (position === undefined) {
    positions.set(event.id, events.length);
    events.push(event);
  } else {
    // The accepted User Event first appears with processed_at=null and later
    // reappears under the same id after commit. Replace that exact occurrence;
    // id-only dedupe would permanently retain the pending receipt.
    const current = events[position];
    if (current?.processed_at && !event.processed_at) return;
    events[position] = event;
  }
}

function reconcileManagedSessionHistory(events, positions, history) {
  const live = new Map(events.map((event) => [event.id, event]));
  events.length = 0;
  positions.clear();
  for (const event of history) {
    const observed = live.get(event.id);
    // A stale replica may still list the accepted receipt as pending after the
    // stream delivered its committed form. Preserve monotonic processed_at
    // while taking canonical ordering from durable history.
    mergeManagedSessionEvent(
      events,
      positions,
      observed?.processed_at && !event.processed_at ? observed : event,
    );
    live.delete(event.id);
  }
  // Events already delivered live but not yet visible to this history replica
  // form the newest suffix until a later reconciliation places them exactly.
  for (const event of live.values()) mergeManagedSessionEvent(events, positions, event);
}

async function waitForSessionEventReceiptStream(
  client,
  sessionId,
  receiptId,
  betas,
  predicate,
  description,
  {
    timeoutMs = 20_000,
    pollMs = 100,
    retryReadError,
    listParams = {},
    streamParams = {},
  },
) {
  const deadline = performance.now() + timeoutMs;
  const events = [];
  const positions = new Map();
  let lastObservation = receiptScopedObservation(events, receiptId);
  let lastRetryableReadError;
  const streamRequest = betas === undefined
    ? { ...streamParams }
    : { ...streamParams, betas };

  const accept = async () => {
    lastObservation = receiptScopedObservation(events, receiptId);
    return Boolean(lastObservation.receiptEvent?.processed_at)
      && await predicate(lastObservation);
  };

  while (performance.now() < deadline) {
    const controller = new AbortController();
    const remaining = Math.max(1, deadline - performance.now());
    // The committed broadcast is process-local. Another API replica can
    // accept and commit the receipt while this SSE connection stays silently
    // open, so reconcile durable history at a bounded cadence even without a
    // disconnect. Keep the same receipt and never replay its Event send.
    const timer = setTimeout(() => controller.abort(), Math.min(remaining, 1_000));
    try {
      const stream = await client.beta.sessions.events.stream(
        sessionId,
        streamRequest,
        { signal: controller.signal },
      );
      for await (const event of stream) {
        mergeManagedSessionEvent(events, positions, event);
        if (await accept()) return lastObservation;
      }
      lastRetryableReadError = undefined;
    } catch (error) {
      if (!controller.signal.aborted) {
        if (retryReadError?.(error) !== true) throw error;
        lastRetryableReadError = error;
      }
    } finally {
      clearTimeout(timer);
    }

    // Streams are turn-bounded and preview delivery is best effort. Reconcile
    // every closed/dropped connection with the persisted record, replacing
    // same-id pending receipts with their committed forms before reconnecting.
    try {
      const history = await listManagedSessionEvents(client, sessionId, betas, listParams);
      reconcileManagedSessionHistory(events, positions, history);
      lastRetryableReadError = undefined;
      if (await accept()) return lastObservation;
    } catch (error) {
      if (retryReadError?.(error) !== true || performance.now() >= deadline) throw error;
      lastRetryableReadError = error;
    }
    if (performance.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, pollMs));
    }
  }
  if (lastRetryableReadError !== undefined) throw lastRetryableReadError;
  throw new Error(`${description}; last observed value: ${stringifyDurableJson(lastObservation)}`);
}

export async function waitForRetainedUnknownExecution(
  client,
  sessionId,
  inputReceiptId,
  betas,
  dispatchDatabase,
  description,
  { cancellationRequested = false } = {},
) {
  // Shared retained-unknown oracle for Native and ACP durable attempts.
  // Causes: C1=accepted input has no applied Dispatch settlement; C2=committed
  // Indeterminate Thread boundary; C3=occupied physical slot; C4=optional
  // explicit cancellation intent. Effects: E1=input stays accepted/unprocessed;
  // E2=terminal unknown is public but aggregate Idle is withheld; E3=the exact
  // slot remains fenced; E4=C4 remains durable. Rules U1=C1+C2+C3=>E1-E3 and
  // U2=U1+C4=>E4. One helper prevents Native/ACP tests from inventing different
  // interpretations of the same Dispatch authority.
  return waitForValue(
    async () => {
      const [session, events] = await Promise.all([
        client.beta.sessions.retrieve(sessionId, { betas }),
        (async () => {
          const listed = [];
          for await (const event of client.beta.sessions.events.list(sessionId, { betas })) {
            listed.push(event);
          }
          return listed;
        })(),
      ]);
      let dispatch = null;
      try {
        [dispatch] = sqliteRows(
          dispatchDatabase,
          `SELECT cancel_requested, active_attempt_owner, active_attempt_epoch, sandbox
             FROM runtime_dispatch
            WHERE thread_id = ?
            ORDER BY created_at DESC
            LIMIT 1`,
          sessionId,
        );
      } catch {
        // The row may be between admission and its first physical claim.
      }
      return { session, events, dispatch };
    },
    ({ session, events, dispatch }) => {
      const inputIndex = events.findIndex((event) => event.id === inputReceiptId);
      if (inputIndex < 0 || events[inputIndex].processed_at !== null) return false;
      const suffix = events.slice(inputIndex + 1);
      return session.status === 'running'
        && suffix.some((event) => event.type === 'session.error'
          && event.error?.retry_status?.type === 'terminal'
          && /execution result is unknown/iu.test(event.error?.message ?? ''))
        && suffix.some((event) => event.type === 'session.thread_status_idle')
        && !suffix.some((event) => event.type === 'session.status_idle')
        && typeof dispatch?.active_attempt_owner === 'string'
        && dispatch.active_attempt_epoch !== null
        && dispatch.sandbox !== null
        && (!cancellationRequested || Number(dispatch.cancel_requested) === 1);
    },
    description,
    { timeoutMs: 30_000, pollMs: 25 },
  );
}

// Canonical receipt-scoped observation for asynchronous Managed Event batches.
// Causes: C1=send returned an exact durable receipt id; C2=history may omit it,
// retain it unprocessed, or commit it later; C3=the caller's scenario effect may
// commit after C2; C4=the caller may supply pagination params and either an
// explicit beta list or no beta so a registry SDK owns its default. Effects:
// E1=return the full last history, the exact committed receipt event, and only
// the later delta; E2=never let older history satisfy C3; E3=forward C4 without
// manufacturing a beta field.
// Constraint: this adapter owns no terminal predicate and never drives the
// Session/Run lifecycle. History polling remains the compatibility default;
// stream-first observation uses the same receipt scope and reconciles every
// closed/dropped stream with authoritative history. Decision rules: W1 !C1=>reject; W2 C1&&!C2
// =>retry; W3 C1+C2&&!C3=>retry; W4 C1+C2+C3=>E1+E2; W5 deadline=>surface the
// last observation through waitForValue; W6 C4=>E3; W7 a caller-classified
// transient history-read failure retries only list; W8 any other read failure
// surfaces immediately; W9 stream events update by id and may satisfy the
// predicate without polling; W10 a closed/dropped stream backfills history and
// reconnects without replaying the accepted command.
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
  const { listParams = {}, preferStream = false, streamParams = {}, ...waitOptions } = options ?? {};
  if (preferStream && typeof client.beta.sessions.events.stream === 'function') {
    return waitForSessionEventReceiptStream(
      client,
      sessionId,
      receiptId,
      betas,
      predicate,
      description,
      { ...waitOptions, listParams, streamParams },
    );
  }
  const sdkListParams = betas === undefined
    ? { ...listParams }
    : { ...listParams, betas };
  return waitForValue(
    async () => {
      const events = await listManagedSessionEvents(client, sessionId, undefined, sdkListParams);
      return receiptScopedObservation(events, receiptId);
    },
    async (observation) => Boolean(observation.receiptEvent?.processed_at)
      && await predicate(observation),
    description,
    waitOptions,
  );
}

// Canonical oracle for a retryable command that failed before it acquired a
// committed projection anchor. Causes: C1=admission returned an exact receipt;
// C2=the root retains that receipt without processing it; C3=history may
// already contain effects from older commands. Effects: E1=events.list exposes
// exactly one pending receipt; E2=only events added after C3 are checked;
// E3=no forbidden execution/terminal effect is added. Decision rules:
// U1 C1+C2=>E1; U2 C1+C2+C3=>E1+E2+E3. The pending receipt is durable command
// provenance, not evidence that its Runtime effect committed.
/**
 * @param {{
 *   history: Array<{ id: string, processed_at?: string | null, type: string }>,
 *   priorHistory?: Array<{ id: string, processed_at?: string | null, type: string }>,
 *   receiptId: string,
 *   forbiddenEventTypes: Set<string>,
 *   description: string,
 * }} observation
 */
export function assertPendingReceiptHasNoRuntimeEffects({
  history,
  priorHistory = [],
  receiptId,
  forbiddenEventTypes,
  description,
}) {
  const matchingReceipts = history.filter((event) => event.id === receiptId);
  if (matchingReceipts.length !== 1) {
    throw new Error(
      `${description} must expose one pending receipt; observed ${matchingReceipts.length}`,
    );
  }
  if (matchingReceipts[0].processed_at !== null) {
    throw new Error(`${description} falsely marked the pending receipt as processed`);
  }
  const priorIds = new Set(priorHistory.map((event) => event.id));
  const added = history.filter((event) => !priorIds.has(event.id));
  if (!added.some((event) => event.id === receiptId)) {
    throw new Error(`${description} did not add the exact pending receipt`);
  }
  const forbidden = added.filter(
    (event) => event.id !== receiptId && forbiddenEventTypes.has(event.type),
  );
  if (forbidden.length > 0) {
    throw new Error(
      `${description} fabricated execution or terminal effects: ${forbidden.map((event) => event.type)}`,
    );
  }
  return added;
}

// Test-authoring factory for scenarios whose causal boundary is an explicit
// confirmation on an official Agent tool. Production defaults stay
// always-allow; a caller names only the tools its decision table needs to gate,
// and the Session override is the sole source of that narrower policy.
export function managedAgentWithAlwaysAskTools(toolNames, agentId = 'assistant') {
  if (!Array.isArray(toolNames) || toolNames.length === 0) {
    throw new Error('managedAgentWithAlwaysAskTools requires at least one tool name');
  }
  if (new Set(toolNames).size !== toolNames.length) {
    throw new Error('managedAgentWithAlwaysAskTools rejects duplicate tool names');
  }
  return {
    id: agentId,
    type: 'agent_with_overrides',
    tools: [{
      type: 'agent_toolset_20260401',
      configs: toolNames.map((name) => ({
        name,
        enabled: true,
        permission_policy: { type: 'always_ask' },
      })),
    }],
  };
}

// One config-plane author/publish operation for E2E fixtures. Scenario helpers
// may choose their own Agent shape, but must reuse this HTTP ownership boundary
// instead of duplicating draft and publication error handling.
export async function publishManagementAgent(baseUrl, agentId, config) {
  if (typeof agentId !== 'string' || agentId.length === 0) {
    throw new Error('publishManagementAgent requires an Agent id');
  }
  if (!config || typeof config !== 'object' || Array.isArray(config)) {
    throw new Error('publishManagementAgent requires an Agent config object');
  }
  const request = async (method, route, body) => {
    const response = await fetch(`${baseUrl}${route}`, {
      method,
      headers: body === undefined ? {} : { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: response.status, body: await response.json().catch(() => ({})) };
  };
  const stored = await request('PUT', `/v1/config/agents/${agentId}`, config);
  if (stored.status !== 200) {
    throw new Error(`store fixture Agent failed: ${JSON.stringify(stored.body)}`);
  }
  const published = await request('POST', `/v1/config/agents/${agentId}/publish`);
  if (published.status !== 200) {
    throw new Error(`publish fixture Agent failed: ${JSON.stringify(published.body)}`);
  }
  return published.body;
}

// Real-provider setup shared by compatibility certification and application
// tasks. Graph: live connection -> discovered offering -> exact credential
// model selection -> publication caller. P1 each successful edge returns the selected
// model; P2 any failed edge or missing explicitly selected model fails closed.
// Secret bytes never enter returned evidence or error messages. Configuration,
// discovery and credential ownership stay in the real Control API.
export async function provisionProviderTestModel(base, workspace, item, profileId, { backendRef = 'genai' } = {}) {
  if (backendRef !== 'genai' && !/^acp:[a-z][a-z0-9-]*$/u.test(backendRef)) {
    throw new Error('Provider test setup requires a Native or ACP backend reference');
  }
  const request = async (method, route, body) => {
    const response = await fetch(`${base}${route}`, {
      method, headers: body === undefined ? {} : { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const result = await response.json();
    if (item.secret && JSON.stringify(result).includes(item.secret)) {
      throw new Error(`${item.id}: provider response exposed credential material`);
    }
    if (!response.ok) throw new Error(`${item.id}: ${route} failed (${response.status})`);
    return result;
  };
  const authentication = item.auth === 'oauth_helper'
    ? { configuration: item.configuration, oauth_helper: item.oauth_helper }
    : { base_url: item.base_url, secret: item.secret };
  const connection = await request('POST', '/v1/config/provider-connections', {
    idempotency_key: `provider-certification-${item.id}`,
    workspace_id: workspace, provider_id: item.provider_id,
    display_name: `Compatibility ${item.id}`, dialect: item.dialect,
    timeout_secs: item.timeout_secs, ...authentication,
  });
  if (!(connection.sync.discovered > 0)) throw new Error(`${item.id}: provider discovered no models`);
  const catalog = await request('GET', '/v1/config/catalog');
  const offerings = catalog.offerings.filter((offering) =>
    offering.provider_id === item.provider_id && offering.protocol_endpoint_id === connection.endpoint.id
      && (offering.status ?? 'active') === 'active');
  const offering = item.model_id
    ? offerings.find((candidate) => candidate.model_id === item.model_id)
    : offerings[0];
  if (!offering) throw new Error(`${item.id}: selected model has no active offering`);
  const target = { model_id: offering.model_id, provider_id: offering.provider_id,
    protocol_endpoint_id: offering.protocol_endpoint_id };
  // Profiles select Native today. ACP must author the existing exact Target
  // via the Managed model-id wire; the internal Target object is not public.
  // Writing a Native profile and relabelling its run
  // would not exercise ACP. Control still owns dialect/capability admission and
  // credential selection, and freezes the selected source in the publication.
  let model = backendRef === 'genai'
    ? offering.model_id
    : `${encodeURIComponent(offering.model_id)};provider=${encodeURIComponent(offering.provider_id)};api=${item.dialect};executor=${backendRef}`;
  // An explicit profile id requests the Config-plane profile variant. Public
  // Managed Agent tests omit it and use the already-supported exact model-id
  // wire selection; do not manufacture an unused parallel profile for them.
  if (backendRef === 'genai' && profileId) {
    await request('PUT', `/v1/config/inference-profiles/${profileId}`, {
      workspace_id: workspace,
      primary: { target, credential_binding: { type: 'exact', credential_source_id: connection.credential.id } },
      fallbacks: [], disabled_endpoint_ids: [],
    });
    model = { mode: 'profile', profile_id: profileId };
  }
  return { offering, endpoint: connection.endpoint, discoveredModels: connection.sync.discovered, model };
}

// Fixture owner for binding one immutable Sandbox execution policy. Endpoint
// conformance tests continue to issue raw requests because they verify the wire;
// behavior scenarios use this helper so policy publication is not reimplemented.
export async function bindSandboxExecutionPolicy(
  baseUrl,
  environmentId,
  { id, provisioning, config = {} },
) {
  const post = async (route, body) => {
    const response = await fetch(`${baseUrl}${route}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    });
    const responseBody = await response.json().catch(() => ({}));
    if (!response.ok) {
      throw new Error(`${route} failed (${response.status}): ${JSON.stringify(responseBody)}`);
    }
    return responseBody;
  };
  const policy = await post('/v1/awaken/sandbox-execution-policies', {
    id,
    config,
    provisioning,
    disabled: false,
  });
  await post(`/v1/awaken/environments/${environmentId}/sandbox-execution-policy`, {
    policy_id: policy.id,
    version: policy.version,
  });
  return policy;
}

// One configuration-authoring fixture for ordinary durable Thread scenarios
// that need a deterministic permission boundary. Causes: C1 Agent id and a
// unique nonempty AlwaysAsk set plus a disjoint optional AlwaysAllow set; C2
// config write succeeds; C3 publication succeeds.
// Effect: E1 the pinned management-probe publication explicitly owns
// the exact permission for each listed tool. Invalid C1 or either failed HTTP
// effect is terminal; callers never fall back to a process default policy.
export async function publishAlwaysAskManagementProbeAgent(
  baseUrl,
  agentId,
  toolNames = ['write'],
  alwaysAllowToolNames = [],
) {
  if (typeof agentId !== 'string' || agentId.length === 0) {
    throw new Error('publishAlwaysAskManagementProbeAgent requires an Agent id');
  }
  if (!Array.isArray(toolNames) || toolNames.length === 0
      || !Array.isArray(alwaysAllowToolNames)
      || new Set([...toolNames, ...alwaysAllowToolNames]).size
        !== toolNames.length + alwaysAllowToolNames.length) {
    throw new Error('publishAlwaysAskManagementProbeAgent requires unique tool names');
  }
  return publishManagementAgent(baseUrl, agentId, {
    name: `AlwaysAsk fixture ${agentId}`,
    model: {
      mode: 'pinned',
      provider_identity_ref: 'default',
      model_ref: 'management-probe',
      backend_ref: 'default',
    },
    tools: [{
      type: 'agent_toolset_20260401',
      configs: [
        ...toolNames.map((name) => ({
          name,
          enabled: true,
          permission_policy: { type: 'always_ask' },
        })),
        ...alwaysAllowToolNames.map((name) => ({
          name,
          enabled: true,
          permission_policy: { type: 'always_allow' },
        })),
      ],
    }],
  });
}

// One application-edge fixture for cross-protocol scenarios that use the real
// AllInOne Config/Agent authority. Causes: C1 no-login management authoring is
// selected by the scenario host; C2 the published Agent can freeze one Managed
// Session; C3 one application token binds the Session's own id on both guarded
// browser protocols. Effects: E1 AI-SDK and AG-UI enter the same Session through
// the production application guard; E2 A2A can address that same canonical id;
// E3 neither the production guard nor Config composition gains a test bypass.
//
// | Rule | C1 | C2 | C3 | Effect |
// | XPA1 | T  | T  | T  | E1 + E2 + E3 |
//
// Authentication-negative combinations remain owned by application_auth_e2e;
// this helper owns only the successful deterministic scenario boundary.
export async function createCrossProtocolApplicationThread(
  baseUrl,
  agentId = 'assistant',
  existingSessionId,
) {
  // XPA2 a real fixture already created its exact Agent/Environment Session:
  // bind that same identity; do not create an unrelated default Session.
  if (existingSessionId !== undefined && (typeof existingSessionId !== 'string' || !existingSessionId.trim())) {
    throw new TypeError('existing application Session id must be non-empty');
  }
  const request = async (route, body) => {
    const response = await fetch(`${baseUrl}${route}`, {
      method: 'POST',
      headers: {
        'anthropic-beta': 'managed-agents-2026-04-01',
        'content-type': 'application/json',
      },
      body: JSON.stringify(body),
    });
    const responseBody = await response.json().catch(() => ({}));
    if (!response.ok) {
      throw new Error(`${route} failed (${response.status}): ${JSON.stringify(responseBody)}`);
    }
    return responseBody;
  };
  const threadId = existingSessionId ?? (await request('/v1/sessions', {
    agent: agentId,
    environment_id: 'env_local',
    title: 'cross-protocol application fixture',
  })).id;
  const capability = await request('/v1/application-access-tokens', {
    protocols: ['ai-sdk', 'ag-ui'],
    operations: ['thread.run', 'thread.messages.read'],
    thread_bindings: [{
      external_thread_id: threadId,
      managed_session_id: threadId,
    }],
    expires_in_seconds: 300,
  });
  if (typeof capability.access_token !== 'string' || !capability.access_token.startsWith('aat_')) {
    throw new Error('application access issuer returned no aat_ capability');
  }
  return {
    threadId,
    accessToken: capability.access_token,
    headers: { authorization: `Bearer ${capability.access_token}` },
  };
}

// Canonical driver for deterministic scenarios whose built-in tools cross one
// or more Managed approval boundaries. Causes: C1=the task receipt commits;
// C2=the latest receipt-scoped boundary is requires_action; C3=its exact tool
// ids have not already been approved; C4=an approval receipt commits; C5=the
// Run reaches end_turn. Effects: E1=approve each qualified tool id exactly
// once; E2=ignore an older requires_action that canonical history orders after
// C4; E3=return terminal committed history. Constraints: this helper drives
// only test-owned allow decisions and never retries a rejected send. Decision
// rules: A1 C1+C2+C3=>E1; A2 C4+C2&&!C3=>E2; A3 C4+C5=>E3; A4 boundary limit
// without C5=>fail with the last committed history.
export async function allowManagedToolBoundaries({
  client,
  sessionId,
  taskReceiptId,
  betas,
  description,
  timeoutMs = 20_000,
  maxBoundaries = 10,
}) {
  const approved = new Set();
  const nextBoundary = ({ events, delta }) => {
    if (hasEndTurn(delta)) return true;
    const idle = [...delta]
      .reverse()
      .find((event) => event.type === 'session.status_idle');
    return idle?.stop_reason?.type === 'requires_action'
      && idle.stop_reason.event_ids.some((id) => !approved.has(id));
  };
  let observation = await waitForSessionEventReceipt(
    client,
    sessionId,
    taskReceiptId,
    betas,
    nextBoundary,
    `${description} task reaches its first committed boundary`,
    { timeoutMs },
  );
  for (let boundary = 0; boundary < maxBoundaries; boundary += 1) {
    if (hasEndTurn(observation.delta)) return observation.events;
    const idle = [...observation.delta]
      .reverse()
      .find((event) => event.type === 'session.status_idle');
    if (idle?.stop_reason?.type !== 'requires_action') {
      throw new Error(`${description} nonterminal boundary does not require approval`);
    }
    const pendingIds = idle.stop_reason.event_ids.filter((id) => !approved.has(id));
    if (pendingIds.length === 0) {
      throw new Error(`${description} requires_action repeats only approved Event ids`);
    }
    const decisions = pendingIds.map((id) => {
      const toolUse = observation.events.find(
        (event) => event.id === id && event.type === 'agent.tool_use',
      );
      if (toolUse?.evaluated_permission !== 'ask') {
        throw new Error(`${description} ${id} is not a gated tool call`);
      }
      return { type: 'user.tool_confirmation', tool_use_id: id, result: 'allow' };
    });
    const approval = await client.beta.sessions.events.send(sessionId, {
      events: decisions,
      betas,
    });
    pendingIds.forEach((id) => approved.add(id));
    const receiptIds = approval.data.map((event) => event.id);
    observation = await waitForSessionEventReceipt(
      client,
      sessionId,
      receiptIds.at(-1),
      betas,
      ({ events, delta }) => receiptIds.every((id) => events.some(
        (event) => event.id === id && event.processed_at,
      )) && nextBoundary({ events, delta }),
      `${description} approval batch reaches its next committed boundary`,
      { timeoutMs },
    );
  }
  throw new Error(
    `${description} did not reach end_turn within ${maxBoundaries} approval boundaries`,
  );
}

// Canonical SessionToolRunner start fence. Causes: C1 an exact task receipt is
// processed; C2 the named custom tool and requires_action are committed after
// C1. Effects: E1 return the official runner's durable reconciliation input;
// E2 never execute or acknowledge the tool. Rules: T1 !C1|!C2=>retry;
// T2 C1+C2=>E1+E2. This observation helper owns no Worker lifecycle state.
export function waitForSessionCustomToolBoundary(
  client,
  sessionId,
  receiptId,
  betas,
  toolName,
  description,
) {
  return waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    betas,
    ({ delta }) => delta.some((event) => (
      event.type === 'agent.custom_tool_use' && event.name === toolName
    )) && [...delta].reverse().find(
      (event) => event.type === 'session.status_idle',
    )?.stop_reason?.type === 'requires_action',
    description,
  );
}

// Canonical settle for an AI SDK `Chat`'s deferred auto-send, shared by every
// HITL scenario rather than copied per file. Causes: C1 the Chat reports
// `ready`; C2 the scenario's terminal effect is present in the committed
// message. Effects: E1 return once both hold; E2 fail with the last observed
// status and reply when they do not. Rules: A1 !C1||!C2=>retry; A2 C1+C2=>E1;
// A3 deadline=>E2.
//
// Testing C1 alone races: `addToolApprovalResponse` resolves while the Chat is
// still `ready`, immediately before the SDK schedules its follow-up request.
// Returning quietly at the deadline is worse than either -- the scenario then
// asserts against a half-finished Chat, and the message blames the effect
// rather than the wait that never happened. `waitForValue` owns the bound, so
// the timeout names what was awaited and shows what was observed instead.
export function waitForChatSettled(chat, effect, description, options = {}) {
  return waitForValue(
    () => ({
      status: chat.status,
      reply: (chat.lastMessage?.parts ?? [])
        .filter((part) => part.type === 'text')
        .map((part) => part.text)
        .join(''),
      parts: (chat.lastMessage?.parts ?? []).map((part) => part.type),
    }),
    (observed) => observed.status === 'ready' && effect(),
    description,
    { timeoutMs: 20_000, pollMs: 100, ...options },
  );
}

export function childDirectories(parent) {
  // Scope-observation rule: C1 a provider-owned Sandbox child; C2 the Namespace
  // provider's private `.awaken-control` rendezvous beside those children.
  // C1 counts as a Sandbox; C2 does not. The exact control directory has its
  // own ownership/permissions tests and must not make a single scope appear
  // ambiguous to every E2E caller of this one inventory helper.
  return fs.existsSync(parent)
    ? fs.readdirSync(parent, { withFileTypes: true })
      .filter((entry) => entry.isDirectory() && entry.name !== '.awaken-control')
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

// Copy one quiesced storage tree for backup/restore and candidate shadow-read
// scenarios. Runtime writers must be stopped before this boundary: a recursive
// copy is intentionally not presented as a live database snapshot protocol.
// Callers may exclude top-level provider realizations: inode-bound sandbox
// directories are execution evidence, not portable backup state, and must be
// rebuilt from the durable Session binding at the restore destination.
export function copyQuiescentTree(
  source,
  destination,
  { excludeTopLevel = [] } = {},
) {
  if (fs.existsSync(destination)) {
    throw new Error(`restore destination already exists: ${destination}`);
  }
  const excluded = new Set(excludeTopLevel);
  fs.cpSync(source, destination, {
    recursive: true,
    errorOnExist: true,
    force: false,
    preserveTimestamps: true,
    filter: (candidate) => {
      const relative = path.relative(source, candidate);
      return relative === '' || !excluded.has(relative.split(path.sep)[0]);
    },
  });
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
      if (observedServer && (observedServer.exitCode !== null || observedServer.signalCode !== null)) {
        finish(terminalError(`code=${observedServer.exitCode}, signal=${observedServer.signalCode}`));
        return;
      }
      // A reachable foreign listener cannot establish this child's readiness.
      // ScenarioHost announces only after its own TcpListener::bind succeeds.
      if (spawnedServerStates.get(observedServer)?.ready === false) {
        if (performance.now() > deadline) finish(new Error(`server did not listen on ${port}`));
        else retry = setTimeout(attempt, 200);
        return;
      }
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

// Default scenarios use offsets within one 100-port block. Probe every member
// using the same socket owner as explicit callers, rather than trusting PID
// modulo uniqueness. A later bind race still fails the owned-child readiness gate.
export async function availablePortBlock(preferred) {
  if (!Number.isInteger(preferred) || preferred < 20_000 || preferred >= 30_000
      || preferred % 100 !== 0) throw new RangeError('invalid E2E port block');
  for (let index = 0; index < 100; index += 1) {
    const base = 20_000 + ((preferred - 20_000 + index * 100) % 10_000);
    let free = true;
    for (let offset = 0; offset < 100; offset += 1) {
      if (await availablePort(base + offset) !== base + offset) {
        free = false;
        break;
      }
    }
    if (free) return base;
  }
  throw new Error('no available E2E port block in 20000..29999');
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
export async function withServer(mode, port, fn, extraEnv = {}) {
  ensureBuilt();
  const listenPort = await availablePort(port);
  // Keep the existing portable substrate default and caller mode precedence.
  const { server, baseUrl } = spawnServer(extraEnv.AWAKEN_MODEL_MODE ?? mode, listenPort, {
    SESSION_DEPLOYMENT_SANDBOX_TIER: 'local',
    ...extraEnv,
  });
  try {
    await waitForServer(server, listenPort);
    return await fn(baseUrl);
  } finally {
    await stopServer(server);
  }
}

// The fake-upstream key the real-wire harness authenticates with, exported so a
// test that spawns the server itself (restart/durability suites) can build the same
// env via `realServerEnv`.
export const FAKE_KEY = 'sk-fake-upstream-key'; // awaken-allow(secret):

// Explicit fake-provider configuration is layered through `extraEnv` after
// inherited provider keys are removed by `serverProcessEnv`. Custom upstream
// fixtures reuse this owner instead of mutating process.env and accidentally
// reclassifying their inert key as ambient input.
export function fakeAnthropicServerEnv(upstream, overrides = {}) {
  return {
    ANTHROPIC_API_KEY: FAKE_KEY,
    ANTHROPIC_BASE_URL: `${upstream.url}/v1/`,
    ANTHROPIC_MODEL: 'fake-haiku',
    ...overrides,
  };
}

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
    ...fakeAnthropicServerEnv(upstream),
    ...extraEnv,
  };
}

export async function withRealServer(behavior, port, fn, opts = {}) {
  ensureBuilt();
  // One authoritative fake-provider fixture owns both inference behavior and
  // optional model discovery; callers must not start a parallel `/v1/models`
  // server merely to seed the live catalog.
  const upstream = await startFakeAnthropic(FAKE_KEY, {
    behavior,
    ...(opts.upstream ?? {}),
  });
  const listenPort = await availablePort(port);
  // When `opts.capture` is set, pipe the child's stdout/stderr so a test can scan
  // the server logs (e.g. the secret-non-leak invariant), teeing them through to
  // this process's streams so behavior is unchanged for a human watching. The
  // accumulated text is exposed to `fn` as a third `{ text() }` argument.
  const capture = opts.capture ? { buf: '' } : null;
  const env = realServerEnv(behavior, upstream, opts);
  const { server, baseUrl } = spawnServer(env.AWAKEN_MODEL_MODE, listenPort, env, process.env, capture);
  try {
    await waitForServer(server, listenPort);
    return await fn(baseUrl, upstream, capture ? { text: () => capture.buf } : null);
  } finally {
    await stopServer(server);
    upstream.close();
  }
}

// Spawn the server without a fixed lifetime, so a test can stop and restart it
// (e.g. to verify durable state survives a process restart). `extraEnv` layers on
// top of the inherited environment — pass `SESSION_DEPLOYMENT_STORAGE_DIR` for durability.
export function spawnServer(mode, port, extraEnv = {}, inheritedEnvironment = process.env, capture = null) {
  const bin = ensureBuilt();
  const addr = `127.0.0.1:${port}`;
  const server = trackSpawnedServer(
    port,
    spawn(bin, {
      env: preparedServerProcessEnv(
        addr,
        { ...extraEnv, AWAKEN_MODEL_MODE: mode },
        inheritedEnvironment,
      ),
      stdio: ['pipe', capture ? 'pipe' : 'inherit', 'pipe'],
    }),
    `awaken-coordinator listening on http://${addr}`,
  );
  // One spawn/capture/readiness owner serves ordinary, real-wire and restart
  // scenarios. Preserve the exact child logs for secret-non-leak assertions.
  const tee = (chunk, sink) => {
    if (capture) capture.buf += chunk.toString();
    sink.write(chunk);
  };
  server.stderr.on('data', chunk => tee(chunk, process.stderr));
  if (capture) server.stdout.on('data', chunk => tee(chunk, process.stdout));
  return { server, baseUrl: `http://${addr}` };
}

// Stop a spawned server and resolve once the process has actually exited, so the
// TCP port is free and the SQLite files are flushed before a restart rebinds.
/**
 * @param {import('node:child_process').ChildProcess} server
 * @param {{requireCleanExit?: boolean}} [options]
 * @returns {Promise<void>}
 */
export async function stopServer(server, { requireCleanExit = false } = {}) {
  const wasLive = server.exitCode === null && server.signalCode === null;
  let closed = false;
  await new Promise((resolve) => {
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
      server.off('exit', onExit);
      server.off('close', onClose);
      server.off('error', finish);
      untrackSpawnedServer(server);
      resolve();
    };
    // Strict acceptance also waits for stdio closure: Worker diagnostics must
    // finish writing before a successful caller deletes the fixture directory.
    const onExit = () => { if (!requireCleanExit) finish(); };
    const onClose = () => { closed = true; finish(); };
    server.once('exit', onExit);
    server.once('close', onClose);
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
  // A terminal child is valid for idempotent fault cleanup, but cannot prove
  // that a healthy deployment survived until its requested graceful shutdown.
  if (requireCleanExit && (!wasLive || !closed || server.exitCode !== 0 || server.signalCode !== null)) {
    throw new Error(`fixture process failed clean shutdown: live_before=${wasLive}; code=${server.exitCode}; signal=${server.signalCode}; stdio_closed=${closed}`);
  }
}

// Cleanup respects dependency order (Worker before Control/transport). Attempt
// every step even after rejection, preserving original and teardown failures.
// Destructive evidence deletion belongs AFTER this function succeeds.
export async function runCleanupSteps(steps, failures = []) {
  const errors = [...failures];
  for (const step of steps) {
    try { await step(); } catch (error) { errors.push(error); }
  }
  if (errors.length) throw new AggregateError(errors, 'fixture cleanup failed');
}

// Shared fault-injection boundary (NOT graceful shutdown). Decision rules:
// live child => SIGKILL exact handle, await exit, unregister; terminal child =>
// unregister idempotently; failed signal/error => fail the scenario. Register
// the exit listener before signalling so fast Windows/Unix exits cannot race it.
export async function hardKillServer(server, rule = 'crash recovery') {
  if (server.exitCode !== null || server.signalCode !== null) {
    await stopServer(server);
    return;
  }
  await new Promise((resolve, reject) => {
    const cleanup = () => { server.off('exit', onExit); server.off('error', onError); };
    const onExit = () => { cleanup(); resolve(); };
    const onError = (error) => { cleanup(); reject(error); };
    server.once('exit', onExit);
    server.once('error', onError);
    if (!server.kill('SIGKILL')) onError(new Error(`${rule}: hard crash signal not delivered`));
  });
  await stopServer(server);
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
