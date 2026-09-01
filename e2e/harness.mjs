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
  cargoScenarioHostBundle,
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
    ...inheritedEnvironment,
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
  productionBin = cargoExecutable({
    cwd: REPO_ROOT,
    packageName: 'awaken-cli',
    targetName: 'awaken',
    prebuiltEnvironmentName: AWAKEN_BIN_ENV,
  });
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
  initializeE2EInstallation(env);
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

// Canonical oracle for a retryable command that failed before it acquired a
// committed projection anchor. Causes: C1=admission returned an exact receipt;
// C2=the root retains that receipt without processing it; C3=history may
// already contain effects from older commands. Effects: E1=events.list exposes
// exactly one pending receipt; E2=only events added after C3 are checked;
// E3=no forbidden execution/terminal effect is added. Decision rules:
// U1 C1+C2=>E1; U2 C1+C2+C3=>E1+E2+E3. The pending receipt is durable command
// provenance, not evidence that its Runtime effect committed.
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
  const request = async (method, route, body) => {
    const response = await fetch(`${baseUrl}${route}`, {
      method,
      headers: body === undefined ? {} : { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: response.status, body: await response.json().catch(() => ({})) };
  };
  const stored = await request('PUT', `/v1/config/agents/${agentId}`, {
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
  if (stored.status !== 200) {
    throw new Error(`store fixture Agent failed: ${JSON.stringify(stored.body)}`);
  }
  const published = await request('POST', `/v1/config/agents/${agentId}/publish`);
  if (published.status !== 200) {
    throw new Error(`publish fixture Agent failed: ${JSON.stringify(published.body)}`);
  }
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
) {
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
  const session = await request('/v1/sessions', {
    agent: agentId,
    environment_id: 'env_local',
    title: 'cross-protocol application fixture',
  });
  const capability = await request('/v1/application-access-tokens', {
    protocols: ['ai-sdk', 'ag-ui'],
    operations: ['thread.run', 'thread.messages.read'],
    thread_bindings: [{
      external_thread_id: session.id,
      managed_session_id: session.id,
    }],
    expires_in_seconds: 300,
  });
  if (typeof capability.access_token !== 'string' || !capability.access_token.startsWith('aat_')) {
    throw new Error('application access issuer returned no aat_ capability');
  }
  return {
    threadId: session.id,
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
    env: preparedServerProcessEnv(addr, {
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
    env: preparedServerProcessEnv(addr, realServerEnv(behavior, upstream, opts)),
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
export function spawnServer(mode, port, extraEnv = {}, inheritedEnvironment = process.env) {
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
