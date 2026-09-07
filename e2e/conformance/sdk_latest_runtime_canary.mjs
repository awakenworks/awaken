// Runtime half of the registry-latest SDK canary. The declaration fingerprint
// detects shape drift; this executable smoke detects generated path, default
// beta, pagination and response-decoding drift against the real Awaken
// topologies that own each resource family.

import assert from 'node:assert/strict';
import {
  existsSync,
  chmodSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { extractOperationsFromPackageRoot } from '../../packages/managed-sdk-oracle/src/extract-operations.mjs';
import {
  auditRequestTypesFromPackageRoot,
  extractResponseContractsFromPackageRoot,
} from '../../packages/managed-sdk-oracle/src/extract-wire-contracts.mjs';
import { managedExportFingerprintFromPackageRoot } from '../../packages/managed-sdk-oracle/src/extract-exports.mjs';
import { managedRuntimeFingerprintFromPackageRoot } from '../../packages/managed-sdk-oracle/src/extract-runtime.mjs';
import { managedTypeFingerprintFromPackageRoot } from '../../packages/managed-sdk-oracle/src/extract-types.mjs';
import { resolveSdkPackage } from '../../packages/managed-sdk-oracle/src/package-source.mjs';
import {
  availablePort,
  deploymentEnv,
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForSessionEventReceipt,
  waitForPort,
  withRealServer,
  withServer,
  withScenarioServer,
} from '../harness.mjs';
import { officialBetaResourceProjection } from './official_sdk_resource_projection.mjs';
import {
  officialSdkCandidateDelta,
  assertRetainedOfficialSdkBehaviorOwners,
  qualifyOfficialSdkCandidateDelta,
} from './official_sdk_candidate_delta.mjs';
import { compileOfficialSdkChangePoints } from './official_sdk_change_point_compile.mjs';
import { exerciseOfficialWebhookContract } from './official_webhook_contract.mjs';
import {
  assertOwnerOperationReceipts,
  recordingFetch,
} from './managed_sdk_operation_receipts.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, '../..');
const packageRoot = process.env.ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT;
assert.ok(packageRoot, 'ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT is required');
const manifest = JSON.parse(readFileSync(resolve(packageRoot, 'package.json'), 'utf8'));
const scope = JSON.parse(readFileSync(
  resolve(REPO, 'packages/managed-sdk-oracle/config/scope.json'),
  'utf8',
));
const oracle = JSON.parse(readFileSync(
  resolve(REPO, 'contracts/anthropic-managed/upstream-oracle.generated.json'),
  'utf8',
));
const candidateDelta = officialSdkCandidateDelta(
  resolveSdkPackage(oracle.current.module).root,
  packageRoot,
  scope,
);
const qualificationCatalog = JSON.parse(readFileSync(
  resolve(HERE, 'official_sdk_candidate_qualifications.json'),
  'utf8',
));
qualifyOfficialSdkCandidateDelta(
  candidateDelta,
  qualificationCatalog.qualifications,
);
const { operations } = extractOperationsFromPackageRoot(packageRoot, scope);
auditRequestTypesFromPackageRoot(
  packageRoot,
  scope,
  operations.map(({ id }) => id),
);
const responseContracts = extractResponseContractsFromPackageRoot(
  packageRoot,
  scope,
  operations.map(({ id }) => id),
);
const completedBehaviorOwners = new Set();
const completeBehaviorOwner = (owner) => completedBehaviorOwners.add(owner);

// Candidate code is imported only after its complete source/type/runtime delta
// matches a reviewed qualification. Extraction above reads package files but
// executes none of them, so a same-path supply-chain mutation fails closed.
const { default: Anthropic, toFile } = await import(pathToFileURL(resolve(packageRoot, 'index.mjs')));
const {
  accumulateManagedAgentsEvent,
} = await import(pathToFileURL(resolve(packageRoot, 'lib/sessions/accumulate.mjs')));
const agentToolset = await import(
  pathToFileURL(resolve(packageRoot, 'tools/agent-toolset/node.mjs')),
);
const {
  betaAgentToolset20260401,
  SessionMemoryStores,
  setupSkills,
} = agentToolset;
const { betaRefusalFallbackMiddleware } = await import(
  pathToFileURL(resolve(packageRoot, 'lib/middleware.mjs'))
);
const managedExports = managedExportFingerprintFromPackageRoot(packageRoot, scope);
const hasResolveSkillVersion = managedExports.exports.some(
  ({ id }) => id === 'tools/agent-toolset/node.mjs#resolveSkillVersion',
);
const betaFiles = officialBetaResourceProjection(operations, 'files');
const betaSkills = officialBetaResourceProjection(operations, 'skills');
const candidateResourceOperations = operations
  .filter(({ id }) => id.startsWith('beta.files.') || id.startsWith('beta.skills.'))
  .map((operation) => Object.freeze({
    sdkMethod: operation.id,
    owner: 'candidate-files-skills',
    method: operation.method,
    route: operation.path,
    transportQuery: operation.transport_query,
    betas: operation.betas,
    responseContract: responseContracts[operation.id],
  }));
const candidateReceipts = [];
const candidateRecordingFetch = recordingFetch(
  globalThis.fetch.bind(globalThis),
  (receipt) => candidateReceipts.push(receipt),
);
// Promotion cause/effect table: C1 reviewed candidate with a nonempty delta;
// C2 the same bytes promoted to current, so delta becomes empty; C3 a module
// remains in the selected SDK's complete Managed dependency closure. E1 C1+C3
// and C2+C3 both execute the same regression owner. Delta qualification still
// owns upgrade admission; it must not decide whether existing behavior is tested.
// The current-version canary exercises C2 with zero delta and real owner calls.
const runtimeFiles = managedRuntimeFingerprintFromPackageRoot(packageRoot, scope).files;
const runtimePaths = new Set(runtimeFiles.map(({ path }) => path));
const hasRuntime = (runtimePath) => runtimePaths.has(runtimePath);
const declarationPaths = new Set(
  managedTypeFingerprintFromPackageRoot(packageRoot, scope).files.map(({ path }) => path),
);
const managedTypeChangePaths = [
  'beta/agents/agents.d.ts',
  'beta/deployment-runs.d.ts',
  'beta/deployments.d.ts',
  'beta/dreams.d.ts',
  'beta/memory-stores/memories.d.ts',
  'beta/sessions/events.d.ts',
  'beta/sessions/sessions.d.ts',
  'beta/user-profiles.d.ts',
];
const managedTypesV123 = managedTypeChangePaths.every((path) => declarationPaths.has(path));
const workspaceRuntimeModules = runtimeFiles
  .map(({ path }) => path)
  .filter((path) => path.startsWith('resources/')
    && /\bworkspace_id\b/u.test(readFileSync(resolve(packageRoot, path), 'utf8')));
const MISSING_FILE_ID = 'file_missing';
const webhookProfile = exerciseOfficialWebhookContract(Anthropic);
completeBehaviorOwner('candidate.webhook-helpers');
compileOfficialSdkChangePoints(packageRoot, {
  filesProjection: betaFiles.projection,
  skillsProjection: betaSkills.projection,
  parseUnverified: webhookProfile.parseUnverified,
  resolveSkillVersion: hasResolveSkillVersion,
  managedTypesV123,
  workspaceRouting: workspaceRuntimeModules.length > 0,
});
completeBehaviorOwner('candidate.typescript-change-points');
if (managedTypesV123) completeBehaviorOwner('candidate.managed-type-change-points');

async function drain(pagePromise) {
  const rows = [];
  for await (const row of pagePromise) rows.push(row);
  return rows;
}

function managedError(status, kind, messagePattern) {
  return (error) => {
    assert.ok(error instanceof Anthropic.APIError, `${status}: official SDK APIError`);
    assert.equal(error.status, status, `${status}: HTTP status`);
    assert.equal(error.error?.type, 'error', `${status}: Anthropic error envelope`);
    assert.equal(error.error?.error?.type, kind, `${status}: Anthropic error kind`);
    assert.equal(typeof error.error?.error?.message, 'string', `${status}: error message`);
    if (messagePattern) assert.match(error.error.error.message, messagePattern);
    return true;
  };
}

function assertCandidateFilesTransport(input, init) {
  const request = input instanceof Request ? input : new Request(input, init);
  const url = new URL(request.url);
  assert.equal(url.pathname.startsWith('/v1/files'), true, 'T1 generated Files route');
  assert.equal(url.searchParams.get('beta'), 'true', 'T1 generated Beta query selector');
  const selectors = (request.headers.get('anthropic-beta') ?? '')
    .split(',')
    .map((value) => value.trim())
    .filter(Boolean);
  if (betaFiles.projection === 'beta') {
    assert.deepEqual(selectors, [betaFiles.capability], 'T1 historical capability selector');
  } else {
    assert.deepEqual(selectors, [], 'T1 post-GA Beta root omits the retired capability');
  }
  assert.equal(request.headers.get('x-api-key'), 'transport-only', 'T1 SDK authentication header');
  assert.equal(
    request.headers.get('user-agent'),
    `Anthropic/JS ${manifest.version}`,
    'T1 exact candidate self-identification',
  );
}

async function exerciseSdkCoreTransport() {
  // Cause/effect graph: C1 exact candidate code constructs a changed Beta
  // Files request; C2 version.mjs supplies transport telemetry. Effects: E1
  // exact route/query/capability/auth and E2 exact candidate self-identity.
  // Error taxonomy, retry decisions, aborts, pagination and complete command
  // identity are owned once by the shared all-anchor transport matrix.
  let requests = 0;
  const client = new Anthropic({
    apiKey: 'transport-only', // awaken-allow: secret
    baseURL: 'https://managed.invalid',
    maxRetries: 0,
    fetch: async (input, init) => {
      requests += 1;
      assertCandidateFilesTransport(input, init);
      return new Response(JSON.stringify(betaFiles.projection === 'beta'
        ? { data: [], has_more: false, first_id: null, last_id: null }
        : { data: [], next_page: null }), {
        headers: { 'content-type': 'application/json' },
      });
    },
  });
  assert.deepEqual(await drain(client.beta.files.list({ limit: 1 })), [], 'T1 page decode');
  assert.equal(requests, 1, 'T1 exact request');
  completeBehaviorOwner('candidate.core-transport');
  pass(`registry SDK ${manifest.version} preserves exact transport identity`);
}

async function exerciseWorkspaceRequestRouting() {
  if (workspaceRuntimeModules.length === 0) return;
  // Cause/effect graph: C1 each Workspace-capable generated Resource module
  // accepts one request-scoped Workspace; C2 credentials may address multiple Workspaces;
  // C3 the selector is absent from query/body transport. Effects: E1 every
  // selected module sends the exact header once; E2 no URL leaks the selector.
  // Decision rule W1=C1+C2+C3=>E1+E2. The module inventory is derived
  // from the selected SDK's complete Managed runtime closure, so a missing or
  // extra probe fails closed.
  const workspaceID = 'workspace_candidate_request';
  const observed = [];
  for (const runtimePath of workspaceRuntimeModules) {
    const source = readFileSync(resolve(packageRoot, runtimePath), 'utf8');
    const destructures = source.match(
      /const \{[^}]*\bworkspace_id\b[^}]*\} = params(?: \?\? \{\})?;/gu,
    ) ?? [];
    const headers = source.match(/'anthropic-workspace-id'/gu) ?? [];
    const guards = source.match(/workspace_id != null/gu) ?? [];
    assert.ok(destructures.length > 0, `${runtimePath}: W1 Workspace selector is absent`);
    assert.equal(headers.length, destructures.length, `${runtimePath}: W1 header coverage`);
    assert.equal(guards.length, destructures.length, `${runtimePath}: W1 omission guard coverage`);
  }
  const client = new Anthropic({
    apiKey: 'transport-only', // awaken-allow: secret
    baseURL: 'https://managed.invalid',
    maxRetries: 0,
    fetch: async (input, init) => {
      const request = input instanceof Request ? input : new Request(input, init);
      const url = new URL(request.url);
      assert.equal(
        request.headers.get('anthropic-workspace-id'),
        workspaceID,
        'W1 exact request-scoped Workspace header',
      );
      assert.equal(url.searchParams.has('workspace_id'), false, 'W1 no Workspace query leak');
      observed.push(url.pathname);
      return new Response(JSON.stringify({
        data: [],
        first_id: null,
        has_more: false,
        last_id: null,
        next_page: null,
      }), {
        headers: {
          'anthropic-workspace-id': workspaceID,
          'content-type': 'application/json',
          'request-id': 'request_workspace_candidate',
        },
      });
    },
  });
  const probes = new Map([
    ['resources/beta/agents/agents.mjs', () => client.beta.agents.list({ workspace_id: workspaceID })],
    ['resources/beta/agents/versions.mjs', () => client.beta.agents.versions.list('agent_candidate', { workspace_id: workspaceID })],
    ['resources/beta/deployment-runs.mjs', () => client.beta.deploymentRuns.list({ workspace_id: workspaceID })],
    ['resources/beta/deployments.mjs', () => client.beta.deployments.list({ workspace_id: workspaceID })],
    ['resources/beta/dreams.mjs', () => client.beta.dreams.list({ workspace_id: workspaceID })],
    ['resources/beta/environments/environments.mjs', () => client.beta.environments.list({ workspace_id: workspaceID })],
    ['resources/beta/environments/work.mjs', () => client.beta.environments.work.stats('env_candidate', { workspace_id: workspaceID })],
    ['resources/beta/files.mjs', () => client.beta.files.list({ workspace_id: workspaceID })],
    ['resources/beta/memory-stores/memories.mjs', () => client.beta.memoryStores.memories.list('memstore_candidate', { workspace_id: workspaceID })],
    ['resources/beta/memory-stores/memory-stores.mjs', () => client.beta.memoryStores.list({ workspace_id: workspaceID })],
    ['resources/beta/memory-stores/memory-versions.mjs', () => client.beta.memoryStores.memoryVersions.list('memstore_candidate', { workspace_id: workspaceID })],
    ['resources/beta/models.mjs', () => client.beta.models.list({ workspace_id: workspaceID })],
    ['resources/beta/sessions/events.mjs', () => client.beta.sessions.events.list('session_candidate', { workspace_id: workspaceID })],
    ['resources/beta/sessions/resources.mjs', () => client.beta.sessions.resources.list('session_candidate', { workspace_id: workspaceID })],
    ['resources/beta/sessions/sessions.mjs', () => client.beta.sessions.list({ workspace_id: workspaceID })],
    ['resources/beta/sessions/threads/events.mjs', () => client.beta.sessions.threads.events.list('thread_candidate', { session_id: 'session_candidate', workspace_id: workspaceID })],
    ['resources/beta/sessions/threads/threads.mjs', () => client.beta.sessions.threads.list('session_candidate', { workspace_id: workspaceID })],
    ['resources/beta/skills/skills.mjs', () => client.beta.skills.list({ workspace_id: workspaceID })],
    ['resources/beta/skills/versions.mjs', () => client.beta.skills.versions.list('skill_candidate', { workspace_id: workspaceID })],
    ['resources/beta/tunnels/certificates.mjs', () => client.beta.tunnels.certificates.list('tunnel_candidate', { workspace_id: workspaceID })],
    ['resources/beta/tunnels/tunnels.mjs', () => client.beta.tunnels.list({ workspace_id: workspaceID })],
    ['resources/beta/vaults/credentials.mjs', () => client.beta.vaults.credentials.list('vault_candidate', { workspace_id: workspaceID })],
    ['resources/beta/vaults/vaults.mjs', () => client.beta.vaults.list({ workspace_id: workspaceID })],
    ['resources/files.mjs', () => client.files.list({ workspace_id: workspaceID })],
    ['resources/models.mjs', () => client.models.list({ workspace_id: workspaceID })],
    ['resources/skills/skills.mjs', () => client.skills.list({ workspace_id: workspaceID })],
    ['resources/skills/versions.mjs', () => client.skills.versions.list('skill_candidate', { workspace_id: workspaceID })],
  ]);
  assert.deepEqual(
    [...probes.keys()].sort(),
    [...workspaceRuntimeModules].sort(),
    'W1 every Workspace-capable generated Resource module has one runtime probe',
  );
  for (const invoke of probes.values()) await invoke();
  assert.equal(observed.length, probes.size, 'W1 every probe reached transport once');
  completeBehaviorOwner('candidate.workspace-request-routing');
  pass(`registry SDK ${manifest.version} preserves request-scoped Workspace routing`);
}

async function exerciseStreamingBufferViews() {
  if (!hasRuntime('core/streaming.mjs')) return;
  // Cause/effect graph: C1 multiple complete SSE frames share one byte buffer;
  // C2 parsing advances through non-copying subarray views. Effects: E1 frames
  // remain exact; E2 ordering is preserved. Decision rule S1=C1+C2=>E1+E2;
  // truncation, alias mutation, duplication, or reordering fails this owner.
  const events = [
    { id: 'event_candidate_1', type: 'session.status_running' },
    { id: 'event_candidate_2', type: 'session.status_idle' },
  ];
  const client = new Anthropic({
    apiKey: 'transport-only', // awaken-allow: secret
    baseURL: 'https://managed.invalid',
    maxRetries: 0,
    fetch: async () => sseResponse(events),
  });
  const observed = [];
  for await (const event of await client.beta.sessions.events.stream('session_candidate')) {
    observed.push(event);
  }
  assert.deepEqual(observed, events, 'S1 multiple events sharing one buffer remain exact and ordered');
  completeBehaviorOwner('candidate.streaming-buffer-views');
  pass(`registry SDK ${manifest.version} preserves SSE framing across buffer views`);
}

async function exerciseSdkNodeRuntimeFacade() {
  const facadePaths = [
    'core/credentials.mjs',
    'internal/file-store.mjs',
    'lib/credentials/credential-chain.mjs',
    'lib/credentials/identity-token.mjs',
    'lib/credentials/types.mjs',
    'lib/credentials/user-oauth.mjs',
    'tools/agent-toolset/fs-util.mjs',
    'tools/agent-toolset/memories.mjs',
    'tools/agent-toolset/skills.mjs',
  ];
  if (!hasRuntime('internal/node.mjs')
    && !facadePaths.some((runtimePath) => hasRuntime(runtimePath))) return;

  // Cause/effect graph: C1 the candidate centralizes every Node builtin behind
  // internal/node; C2 credential/profile readers, confined file stores, atomic
  // tool writes, Memory mounts, and Skill setup consume that facade. Effects:
  // E1 exact module capabilities remain callable; E2 credentials remain private
  // and resolve from the selected profile; E3 file/memory bytes and hashes are
  // unchanged. Decision rules N1 C1+C2->E1+E2+E3; N2 a missing facade export,
  // browser stub leak, path error, or changed byte result fails this owner.
  const nodeFacade = await import(pathToFileURL(resolve(packageRoot, 'internal/node.mjs')));
  assert.equal(typeof nodeFacade.fs.promises.readFile, 'function', 'N1 Node fs facade');
  assert.equal(typeof nodeFacade.crypto.createHash, 'function', 'N1 Node crypto facade');
  assert.equal(typeof nodeFacade.path.join, 'function', 'N1 Node path facade');
  assert.equal(typeof nodeFacade.stream.promises.pipeline, 'function', 'N1 Node stream facade');
  assert.equal(typeof nodeFacade.child_process.execFile, 'function', 'N1 Node process facade');
  assert.equal(typeof nodeFacade.util.promisify, 'function', 'N1 Node util facade');

  const { openFileStore } = await import(
    pathToFileURL(resolve(packageRoot, 'internal/file-store.mjs'))
  );
  const { defaultCredentials } = await import(
    pathToFileURL(resolve(packageRoot, 'lib/credentials/credential-chain.mjs'))
  );
  const { identityTokenFromFile } = await import(
    pathToFileURL(resolve(packageRoot, 'lib/credentials/identity-token.mjs'))
  );
  const { writeCredentialsFileAtomic } = await import(
    pathToFileURL(resolve(packageRoot, 'lib/credentials/types.mjs'))
  );
  const { atomicWriteFile, DIR_CREATE_MODE, FILE_CREATE_MODE } = await import(
    pathToFileURL(resolve(packageRoot, 'tools/agent-toolset/fs-util.mjs'))
  );
  const root = mkdtempSync(resolve(tmpdir(), 'awaken-candidate-node-facade-'));
  const previousConfigDirectory = process.env.ANTHROPIC_CONFIG_DIR;
  try {
    const credentialsDirectory = resolve(root, 'credentials');
    const credentialsPath = resolve(credentialsDirectory, 'fixture.json');
    const identityPath = resolve(root, 'identity-token');
    mkdirSync(resolve(root, 'configs'), { recursive: true });
    await writeCredentialsFileAtomic(credentialsPath, {
      version: '1.0',
      type: 'oauth_token',
      access_token: 'candidate-oauth-token', // awaken-allow: secret
    });
    writeFileSync(identityPath, 'candidate-identity-token\n', { mode: 0o600 });
    writeFileSync(resolve(root, 'configs/fixture.json'), JSON.stringify({
      version: '1.0',
      authentication: { type: 'user_oauth' },
      base_url: 'https://candidate.invalid',
      workspace_id: 'workspace_candidate',
    }), { mode: 0o600 });
    process.env.ANTHROPIC_CONFIG_DIR = root;
    const credentials = await defaultCredentials({
      baseURL: 'https://ignored.invalid',
      fetch: globalThis.fetch,
      userAgent: 'candidate-node-canary',
    }, 'fixture');
    assert.ok(credentials, 'N1 selected profile resolves credentials');
    assert.equal(credentials.baseURL, 'https://candidate.invalid', 'N1 profile API host');
    assert.deepEqual(credentials.extraHeaders, {
      'anthropic-workspace-id': 'workspace_candidate',
    }, 'N1 profile Workspace header');
    assert.deepEqual(await credentials.provider(), {
      token: 'candidate-oauth-token', // awaken-allow: secret (synthetic canary fixture)
      expiresAt: null,
    }, 'N1 user OAuth file result');
    assert.equal(
      await identityTokenFromFile(identityPath)(),
      'candidate-identity-token',
      'N1 rotating identity-token file result',
    );

    const store = await openFileStore(resolve(root, 'store'), { utf8: true });
    await store.createRoot();
    await store.put('/nested/value.txt', 'candidate-bytes');
    assert.equal(
      new TextDecoder().decode(await store.get('/nested/value.txt')),
      'candidate-bytes',
      'N1 confined FileStore bytes',
    );
    assert.equal(
      await store.hashFile('/nested/value.txt'),
      nodeFacade.crypto.createHash('sha256').update('candidate-bytes').digest('hex'),
      'N1 FileStore uses the facade hash without semantic drift',
    );
    await store.dispose();

    const atomicPath = resolve(root, 'atomic.txt');
    await atomicWriteFile(atomicPath, 'atomic-candidate');
    assert.equal(readFileSync(atomicPath, 'utf8'), 'atomic-candidate', 'N1 atomic tool write');
    if (hasRuntime('tools/agent-toolset/fs-util.mjs')) {
      assert.equal(DIR_CREATE_MODE, 0o700, 'N2 owner-only directory creation');
      assert.equal(FILE_CREATE_MODE, 0o600, 'N2 owner-only file creation');
      const privateDirectory = resolve(root, 'private-directory');
      await nodeFacade.fs.promises.mkdir(privateDirectory, { mode: DIR_CREATE_MODE });
      assert.equal(statSync(privateDirectory).mode & 0o777, 0o700, 'N2 new directory mode');
      assert.equal(statSync(atomicPath).mode & 0o777, 0o600, 'N2 new file mode');
      chmodSync(atomicPath, 0o750);
      await atomicWriteFile(atomicPath, 'atomic-preserved-mode');
      assert.equal(statSync(atomicPath).mode & 0o777, 0o750, 'N2 existing mode survives rename');
      completeBehaviorOwner('candidate.agent-toolset-file-permissions');
    }

    const memoryClient = {
      beta: { memoryStores: { memories: {
        list() {
          return { async *[Symbol.asyncIterator]() {} };
        },
      } } },
    };
    const memories = new SessionMemoryStores(memoryClient, { workdir: root });
    await memories.download({
      resources: [{
        type: 'memory_store',
        memory_store_id: 'mem_candidate',
        name: 'candidate',
        access: 'read_write',
      }],
    });
    assert.equal(
      readFileSync(resolve(root, 'memory/candidate/.anthropic-memory-store'), 'utf8'),
      'version 1\nmem_candidate',
      'N1 Memory helper writes the canonical marker through FileStore',
    );
    await memories.dispose();
  } finally {
    if (previousConfigDirectory === undefined) delete process.env.ANTHROPIC_CONFIG_DIR;
    else process.env.ANTHROPIC_CONFIG_DIR = previousConfigDirectory;
    rmSync(root, { recursive: true, force: true });
  }
  completeBehaviorOwner('candidate.node-runtime-facade');
  pass(`registry SDK ${manifest.version} preserves the centralized Node runtime facade`);
}

function sseResponse(events) {
  return new Response(events.map((event) => (
    `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`
  )).join(''), { headers: { 'content-type': 'text/event-stream' } });
}

async function exerciseRefusalFallbackMiddleware() {
  if (!hasRuntime('lib/middleware.mjs')) return;

  // Cause/effect decision table:
  // | input history / fallback hop                 | expected effect |
  // | already-empty assistant message             | preserved       |
  // | fallback-only replay message                | removed         |
  // | accepted streaming hop start transformation | terminal delta  |
  // C1/C2 separate an intentional empty payload from one emptied by stripping;
  // C3 the fallback message_start is suppressed during stream splicing. E1
  // prevents data loss, E2 prevents replaying private seam blocks, and E3 keeps
  // input_transformations observable on the one public terminal event.
  const logger = { debug() {}, info() {}, warn() {}, error() {} };
  const history = [
    { role: 'assistant', content: [], output_config: { effort: 'high' } },
    { role: 'assistant', content: [{ type: 'fallback', from: {}, to: {}, trigger: {} }] },
    { role: 'user', content: [{ type: 'text', text: 'continue' }] },
  ];
  const bodies = [];
  const nonStreaming = betaRefusalFallbackMiddleware([{ model: 'fallback-model' }]);
  const nonStreamingResponse = await nonStreaming(
    { headers: new Headers(), body: JSON.stringify({ model: 'primary-model', messages: history }) },
    async (request) => {
      bodies.push(JSON.parse(request.body));
      if (bodies.length === 1) {
        return new Response(JSON.stringify({
          type: 'message',
          model: 'primary-model',
          content: [],
          stop_reason: 'refusal',
          stop_details: {
            type: 'refusal',
            category: 'policy',
            fallback_credit_token: 'credit-non-streaming', // awaken-allow: secret (synthetic canary fixture)
          },
        }), { headers: { 'content-type': 'application/json' } });
      }
      return new Response(JSON.stringify({
        type: 'message',
        model: 'fallback-model',
        content: [{ type: 'text', text: 'served' }],
        stop_reason: 'end_turn',
      }), { headers: { 'content-type': 'application/json' } });
    },
    {
      logger,
      options: {
        method: 'post',
        path: '/v1/messages?beta=true',
        body: { model: 'primary-model', messages: history },
        stream: false,
      },
      parse: (response) => response.clone().json(),
    },
  );
  assert.equal((await nonStreamingResponse.json()).content[0].type, 'fallback', 'M1 seam block');
  assert.equal(bodies.length, 2, 'M1 exactly one fallback request');
  assert.deepEqual(
    bodies[1].messages,
    [history[0], history[2]],
    'M1 preserves an originally empty message and removes a fallback-only replay',
  );

  const transformations = [{ type: 'candidate_input_transformation' }];
  let streamHop = 0;
  const streaming = betaRefusalFallbackMiddleware([{ model: 'fallback-model' }]);
  const spliced = await streaming(
    {
      headers: new Headers(),
      body: JSON.stringify({ model: 'primary-model', messages: [history[2]], stream: true }),
    },
    async () => {
      streamHop += 1;
      if (streamHop === 1) {
        return sseResponse([
          { type: 'message_start', message: {
            model: 'primary-model', usage: { input_tokens: 1, output_tokens: 0 },
          } },
          { type: 'message_delta', delta: {
            stop_reason: 'refusal',
            stop_sequence: null,
            stop_details: {
              type: 'refusal',
              category: 'policy',
              fallback_credit_token: 'credit-streaming', // awaken-allow: secret (synthetic canary fixture)
              fallback_has_prefill_claim: false,
            },
          }, usage: { output_tokens: 1 } },
          { type: 'message_stop' },
        ]);
      }
      return sseResponse([
        { type: 'message_start', message: {
          model: 'fallback-model',
          usage: { input_tokens: 2, output_tokens: 0 },
          input_transformations: transformations,
        } },
        { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } },
        { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'served' } },
        { type: 'content_block_stop', index: 0 },
        { type: 'message_delta', delta: {
          stop_reason: 'end_turn', stop_sequence: null,
        }, usage: { output_tokens: 1 } },
        { type: 'message_stop' },
      ]);
    },
    {
      logger,
      options: {
        method: 'post',
        path: '/v1/messages?beta=true',
        body: { model: 'primary-model', messages: [history[2]], stream: true },
        stream: true,
      },
      parse: (response) => response.clone().json(),
    },
  );
  const events = (await spliced.text()).split('\n')
    .filter((line) => line.startsWith('data: '))
    .map((line) => JSON.parse(line.slice(6)));
  const terminal = events.find((event) => event.type === 'message_delta'
    && event.delta?.stop_reason === 'end_turn');
  assert.deepEqual(terminal?.input_transformations, transformations, 'M2 spliced transformation');
  assert.equal(streamHop, 2, 'M2 exactly one streaming fallback request');
  completeBehaviorOwner('candidate.refusal-fallback-middleware');
  pass(`registry SDK ${manifest.version} preserves refusal fallback history and stream metadata`);
}

async function exerciseSdkHelperRuntime() {
  // Dependency-closure cause/effect graph: C1 candidate Session accumulation,
  // C2 Agent Toolset, C3 cross-realm fetch errors, and C4 SSE parsing may change
  // without changing a Managed HTTP operation or declaration. Effects: E1 the
  // known message snapshot is canonical and a future event is non-destructive;
  // E2 every tool remains registered and a bounded range can read a large file;
  // E3 foreign AbortError is classified as a timeout; E4 malformed SSE uses the
  // configured logger; E5 new files/directories are owner-only while an edit
  // preserves the existing mode. A changed implementation runs its enduring behavior
  // assertion; unchanged implementations still run their shared smoke path.
  let accumulated = accumulateManagedAgentsEvent(undefined, {
    type: 'event_start',
    event: { id: 'message_candidate', type: 'agent.message' },
  });
  accumulated = accumulateManagedAgentsEvent(accumulated, {
    type: 'event_delta',
    event_id: 'message_candidate',
    delta: { index: 0, content: { type: 'text', text: 'candidate' } },
  });
  assert.equal(accumulated.content[0].text, 'candidate', 'H1 known Session accumulation');
  if (hasRuntime('lib/sessions/accumulate.mjs')) {
    const future = { type: 'session.future_event', id: 'future_candidate' };
    assert.equal(
      accumulateManagedAgentsEvent(accumulated, future),
      accumulated,
      'H1 unknown future event preserves the accumulated snapshot',
    );
  }

  const workdir = mkdtempSync(resolve(tmpdir(), 'awaken-candidate-toolset-'));
  try {
    const tools = betaAgentToolset20260401({ workdir, maxFileBytes: 32 });
    assert.deepEqual(
      tools.map(({ name }) => name).sort(),
      ['bash', 'edit', 'glob', 'grep', 'read', 'write'],
      'H2 complete Agent Toolset registration',
    );
    if (hasRuntime('tools/agent-toolset/node.mjs')) {
      assert.equal(
        Object.hasOwn(agentToolset, 'resolveSkillVersion'),
        false,
        'H2 0.122 explicitly removes the former public resolveSkillVersion helper',
      );
      const lines = Array.from({ length: 40 }, (_, index) => `line-${String(index + 1).padStart(2, '0')}`);
      writeFileSync(join(workdir, 'large.txt'), lines.join('\n'));
      const read = tools.find(({ name }) => name === 'read');
      const edit = tools.find(({ name }) => name === 'edit');
      assert.equal(
        await read.run({ file_path: 'large.txt', view_range: [20, 20] }),
        'line-20',
        'H2 bounded range streams one line from an over-limit file',
      );
      await assert.rejects(
        () => read.run({ file_path: 'large.txt' }),
        /exceeds 32-byte limit/u,
        'H2 whole over-limit read fails closed',
      );
      await assert.rejects(
        () => read.run({ file_path: 'large.txt', view_range: [1] }),
        /view_range must be \[start_line, end_line\]/u,
        'H2 malformed range fails before filesystem work',
      );
      await assert.rejects(
        () => edit.run({ file_path: 'large.txt', old_string: 'line', new_string: 'changed' }),
        /exceeds 32-byte limit/u,
        'H2 edit retains its whole-file safety bound',
      );
      completeBehaviorOwner('candidate.agent-toolset-node');
    }
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }

  if (hasRuntime('internal/errors.mjs')) {
    const foreignAbort = {
      name: 'AbortError',
      message: 'foreign abort',
      [Symbol.toStringTag]: 'DOMException',
    };
    const client = new Anthropic({
      apiKey: 'transport-only', // awaken-allow: secret
      baseURL: 'https://managed.invalid',
      maxRetries: 0,
      fetch: async (input, init) => {
        assertCandidateFilesTransport(input, init);
        throw foreignAbort;
      },
    });
    await assert.rejects(
      () => client.beta.files.retrieveMetadata('file_transport'),
      (error) => error instanceof Anthropic.APIConnectionTimeoutError,
      'H3 cross-realm DOMException is classified as a timeout',
    );
    completeBehaviorOwner('candidate.error-classification');
  }

  if (hasRuntime('internal/uploads.mjs')) {
    const requestUrls = [];
    const client = new Anthropic({
      apiKey: 'transport-only', // awaken-allow: secret
      baseURL: 'https://managed.invalid',
      maxRetries: 0,
      fetch: async (input) => {
        const url = input instanceof Request ? input.url : String(input);
        requestUrls.push(url);
        if (url === 'data:,') return new Response('');
        throw new Error('invalid upload reached transport');
      },
    });
    const create = (files) => client.beta.skills.create({
      ...(betaSkills.projection === 'beta'
        ? { display_title: 'Invalid Upload' }
        : { display_name: 'Invalid Upload' }),
      files,
    });
    await assert.rejects(
      () => create([new Uint8Array([1])]),
      /wrap them with `await toFile/u,
      'H5 raw bytes explain the strongly typed upload path',
    );
    await assert.rejects(
      () => create([Promise.resolve(new Blob(['content']))]),
      /await it first/u,
      'H5 unresolved upload Promise explains the missing await',
    );
    assert.ok(
      requestUrls.every((url) => url === 'data:,'),
      'H5 invalid upload values fail before API transport',
    );
  }

  if (hasRuntime('core/middleware.mjs') || hasRuntime('internal/parse.mjs')) {
    const errors = [];
    const logger = {
      debug() {},
      info() {},
      warn() {},
      error(...values) { errors.push(values); },
    };
    const client = new Anthropic({
      apiKey: 'transport-only', // awaken-allow: secret
      baseURL: 'https://managed.invalid',
      logger,
      logLevel: 'debug',
      maxRetries: 0,
      fetch: async () => new Response('event: agent.message\ndata: {invalid\n\n', {
        headers: { 'content-type': 'text/event-stream' },
      }),
    });
    const stream = await client.beta.sessions.events.stream('session_transport');
    await assert.rejects(async () => {
      for await (const event of stream) void event;
    }, SyntaxError, 'H4 malformed candidate SSE fails parsing');
    assert.ok(
      errors.some((values) => values.some((value) => String(value).includes('{invalid'))),
      'H4 configured logger receives malformed SSE evidence',
    );
    completeBehaviorOwner('candidate.sse-parser');
  }
  pass(`registry SDK ${manifest.version} preserves Managed helper dependency semantics`);
}

async function exerciseBetaFiles(client) {
  // Cause/effect graph: C6 the official generated Beta Files operations carry
  // their historical capability header; C7 they retain beta=true but carry no
  // capability after GA. Effects: E6 decode Beta metadata/Page; E7 decode GA
  // metadata/PageCursor while staying under client.beta.files; E8 every method
  // reaches the one File authority. Decision rules F1 C6->E6+E8;
  // F2 C7->E7+E8. The generated operation inventory, not an SDK version branch,
  // selects the request and assertions.
  const file = await client.beta.files.upload({
    file: await toFile(Buffer.from(manifest.version), 'latest-beta.txt'),
    ...(betaFiles.projection === 'ga' ? { expires_in_seconds: 3_600 } : {}),
  });
  const peer = await client.beta.files.upload({
    file: await toFile(Buffer.from('peer'), 'latest-beta-peer.txt'),
    ...(betaFiles.projection === 'ga' ? { expires_in_seconds: 3_600 } : {}),
  });
  assert.equal(file.type, 'file', 'F1/F2 shared File identity');
  assert.equal(file.filename, 'latest-beta.txt', 'F1/F2 metadata');
  if (betaFiles.projection === 'beta') {
    assert.equal(Object.hasOwn(file, 'expires_at'), false, 'F1/E6');
  } else {
    assert.equal(typeof file.expires_at, 'string', 'F2/E7');
    assert.equal(file.scope, null, 'F2/E7 Beta namespace retains nullable scope');
  }
  assert.equal((await client.beta.files.retrieveMetadata(file.id)).id, file.id, 'F1/F2 retrieve');
  const listed = await drain(client.beta.files.list({ limit: 1 }));
  assert.deepEqual(
    new Set(listed.map(({ id }) => id)),
    new Set([file.id, peer.id]),
    'F1/F2 cursor traversal returns every File exactly once',
  );
  if (betaFiles.projection === 'ga') {
    const selected = await drain(client.beta.files.list({ ids: [file.id, MISSING_FILE_ID] }));
    assert.deepEqual(selected.map(({ id }) => id), [file.id], 'F2/E7 ids[]');
    await assert.rejects(
      async () => client.beta.files.upload({
        file: await toFile(Buffer.from('invalid'), 'invalid-expiry.txt'),
        expires_in_seconds: 3_599,
      }),
      managedError(400, 'invalid_request_error'),
      'F2 invalid expiry fails before mutation',
    );
    await assert.rejects(
      () => drain(client.beta.files.list({ ids: [file.id], limit: 1 })),
      managedError(400, 'invalid_request_error'),
      'F2 mutually exclusive pagination selectors fail closed',
    );
  }
  await assert.rejects(
    () => client.beta.files.retrieveMetadata(MISSING_FILE_ID),
    managedError(404, 'not_found_error'),
    'F1/F2 unknown File metadata',
  );
  await assert.rejects(
    () => client.beta.files.download(file.id),
    managedError(400, 'invalid_request_error', /not downloadable/u),
    'F1/F2 input download policy',
  );
  assert.equal((await client.beta.files.delete(file.id)).type, 'file_deleted', 'F1/F2 delete');
  await assert.rejects(
    () => client.beta.files.retrieveMetadata(file.id),
    managedError(404, 'not_found_error'),
    'F1/F2 deleted File stays absent',
  );
  await assert.rejects(
    () => client.beta.files.delete(file.id),
    managedError(404, 'not_found_error'),
    'F1/F2 repeated delete is not falsely idempotent',
  );
  await client.beta.files.delete(peer.id);
  completeBehaviorOwner('candidate.files-runtime');
}

async function exerciseGeneratedFileDownload() {
  // Files.download has two semantically distinct paths: uploaded inputs must
  // fail closed, while an Agent-produced artifact must return bytes. The Files
  // canary above owns the former. This isolated Dream topology supplies a legal
  // generated artifact for the latter without manufacturing a test-only File
  // record or granting uploads a production-invalid capability. Dream remains
  // owned by its canonical behavior scenario; it is fixture setup here.
  await withServer('dream', Number(process.env.E2E_PORT), async (baseURL) => {
    const client = new Anthropic({
      apiKey: 'e2e-dummy',
      baseURL,
      fetch: candidateRecordingFetch,
    });
    const sourceSession = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      initial_events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `downloadable-candidate-${manifest.version}` }],
      }],
    });
    for (let attempt = 0; attempt < 400; attempt += 1) {
      const source = await client.beta.sessions.retrieve(sourceSession.id);
      if (source.status === 'idle') break;
      assert.notEqual(source.status, 'terminated', 'generated File source Session remains viable');
      if (attempt === 399) assert.fail('generated File source Session did not become idle');
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    const sourceStore = await client.beta.memoryStores.create({
      name: `downloadable-candidate-${manifest.version}`,
    });
    const target = await client.beta.dreams.create({
      inputs: [
        { type: 'memory_store', memory_store_id: sourceStore.id },
        { type: 'sessions', session_ids: [sourceSession.id] },
      ],
      model: 'claude-sonnet-5',
      instructions: '[awaken-test:hold-dream-for-cancellation]',
    });
    for (let attempt = 0; attempt < 400; attempt += 1) {
      const dream = await client.beta.dreams.retrieve(target.id);
      if (dream.status === 'running' && dream.session_id && dream.outputs.length === 1) break;
      assert.ok(
        !['completed', 'failed', 'canceled'].includes(dream.status),
        `generated File Dream became ${dream.status} before preparation`,
      );
      if (attempt === 399) assert.fail('generated File Dream did not expose preparation');
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    const files = await drain(client.beta.files.list());
    const transcript = files.find(({ filename }) => filename === `${sourceSession.id}.jsonl`);
    assert.ok(transcript, 'candidate setup publishes one generated transcript File');
    assert.equal(transcript.downloadable, true, 'candidate transcript carries output capability');
    const betaBytes = await (await client.beta.files.download(transcript.id)).text();
    assert.ok(betaBytes.includes(`downloadable-candidate-${manifest.version}`));
    assert.equal(
      await (await client.files.download(transcript.id)).text(),
      betaBytes,
      'candidate Beta and GA download methods decode the same generated bytes',
    );
    assert.equal((await client.beta.dreams.cancel(target.id)).status, 'canceled');
    await client.beta.dreams.archive(target.id);
    await client.beta.sessions.delete(sourceSession.id);
    await client.beta.memoryStores.archive(sourceStore.id);
    await client.beta.memoryStores.delete(sourceStore.id);
    pass(`registry SDK ${manifest.version} downloads one generated File through Beta and GA roots`);
  });
}

async function exerciseBetaSkills(client) {
  // Cause/effect graph: S1 generated Beta Skills operations carry the Skills
  // capability; S2 post-GA operations keep beta=true without it. Effects:
  // E1 historical display_title/latest_version/version projection; E2 GA
  // display_name/latest_version_id/id projection; E3 all nine generated methods,
  // including archive download, share one SkillStore lifecycle. Decision rules:
  // S1->E1+E3; S2->E2+E3. The operation signature is the only discriminator.
  const document = '---\nname: latest-beta-canary\ndescription: first\n---\nFirst.';
  const revised = '---\nname: latest-beta-canary\ndescription: second\n---\nSecond.';
  if (hasRuntime('internal/uploads.mjs')) {
    await assert.rejects(
      () => client.beta.skills.create({
        ...(betaSkills.projection === 'beta'
          ? { display_title: 'Bare Blob' }
          : { display_name: 'Bare Blob' }),
        files: [new Blob([document], { type: 'text/markdown' })],
      }),
      managedError(400, 'invalid_request_error', /SKILL\.md/u),
      'S0 candidate bare Blob crosses multipart encoding and fails at bundle validation',
    );
  }
  const skill = await client.beta.skills.create({
    ...(betaSkills.projection === 'beta'
      ? { display_title: 'Latest Beta Canary' }
      : { display_name: 'Latest Beta Canary' }),
    files: [await toFile(Buffer.from(document), 'SKILL.md')],
  });
  assert.equal(skill.type, 'skill', 'S1/S2 create');
  const firstVersion = betaSkills.projection === 'beta'
    ? skill.latest_version
    : skill.latest_version_id;
  assert.equal(typeof firstVersion, 'string', 'S1/S2 initial version');
  if (betaSkills.projection === 'beta') {
    assert.equal(skill.display_title, 'Latest Beta Canary', 'S1/E1');
    assert.equal(Object.hasOwn(skill, 'display_name'), false, 'S1/E1');
  } else {
    assert.equal(skill.display_name, 'Latest Beta Canary', 'S2/E2');
    assert.equal(skill.source.type, 'custom', 'S2/E2');
    assert.equal(Object.hasOwn(skill, 'display_title'), false, 'S2/E2');
  }
  assert.equal((await client.beta.skills.retrieve(skill.id)).id, skill.id, 'S1/S2 retrieve');
  await assert.rejects(
    async () => client.beta.skills.create({
      ...(betaSkills.projection === 'beta'
        ? { display_title: 'Duplicate' }
        : { display_name: 'Duplicate' }),
      files: [await toFile(Buffer.from(document), 'SKILL.md')],
    }),
    managedError(409, 'conflict_error'),
    'S1/S2 duplicate durable Skill identity',
  );
  const peerDocument = '---\nname: latest-beta-peer\ndescription: peer\n---\nPeer.';
  const peer = await client.beta.skills.create({
    ...(betaSkills.projection === 'beta'
      ? { display_title: 'Latest Beta Peer' }
      : { display_name: 'Latest Beta Peer' }),
    files: [await toFile(Buffer.from(peerDocument), 'SKILL.md')],
  });
  const skills = await drain(client.beta.skills.list({ limit: 1 }));
  assert.deepEqual(
    new Set(skills.map(({ id }) => id)),
    new Set([skill.id, peer.id]),
    'S1/S2 Skill cursor traversal returns every identity exactly once',
  );
  await assert.rejects(
    () => client.beta.skills.retrieve('skill_missing'),
    managedError(404, 'not_found_error'),
    'S1/S2 unknown Skill',
  );
  await assert.rejects(
    () => drain(client.beta.skills.versions.list('skill_missing')),
    managedError(404, 'not_found_error'),
    'S1/S2 unknown Skill Version collection',
  );

  const version = await client.beta.skills.versions.create(skill.id, {
    files: [await toFile(Buffer.from(revised), 'SKILL.md')],
  });
  const versionReference = betaSkills.projection === 'beta' ? version.version : version.id;
  assert.equal(typeof versionReference, 'string', 'S1/S2 version create');
  assert.equal(
    (await client.beta.skills.versions.retrieve(versionReference, { skill_id: skill.id })).skill_id,
    skill.id,
    'S1/S2 version retrieve',
  );
  const versions = await drain(client.beta.skills.versions.list(skill.id, { limit: 1 }));
  assert.deepEqual(
    new Set(versions.map((item) => (
      betaSkills.projection === 'beta' ? item.version : item.id
    ))),
    new Set([firstVersion, versionReference]),
    'S1/S2 Version cursor traversal returns every identity exactly once',
  );
  const archive = await client.beta.skills.versions.download(versionReference, {
    skill_id: skill.id,
  });
  assert.match(await archive.text(), /Second\./u, 'S1/S2 archive download');
  await assert.rejects(
    () => client.beta.skills.versions.retrieve('version_missing', { skill_id: skill.id }),
    managedError(404, 'not_found_error'),
    'S1/S2 unknown immutable Version',
  );
  await assert.rejects(
    () => client.beta.skills.versions.download('version_missing', { skill_id: skill.id }),
    managedError(404, 'not_found_error'),
    'S1/S2 unknown Version archive',
  );
  assert.equal(
    (await client.beta.skills.versions.delete(firstVersion, { skill_id: skill.id })).type,
    'skill_version_deleted',
    'S1/S2 version delete',
  );
  await assert.rejects(
    () => client.beta.skills.versions.delete(firstVersion, { skill_id: skill.id }),
    managedError(404, 'not_found_error'),
    'S1/S2 retired Version stays absent',
  );
  await assert.rejects(
    () => client.beta.skills.versions.delete(versionReference, { skill_id: skill.id }),
    managedError(400, 'invalid_request_error'),
    'S1/S2 last live Version protects the aggregate invariant',
  );
  assert.equal((await client.beta.skills.delete(skill.id)).type, 'skill_deleted', 'S1/S2 delete');
  await assert.rejects(
    () => client.beta.skills.delete(skill.id),
    managedError(404, 'not_found_error'),
    'S1/S2 repeated Skill delete',
  );
  await client.beta.skills.delete(peer.id);
  completeBehaviorOwner('candidate.skills-runtime');
  if (hasRuntime('internal/uploads.mjs')) {
    completeBehaviorOwner('candidate.upload-admission');
  }
}

async function exerciseBetaGaRecovery() {
  // Cause/effect graph: C1 the candidate Beta root selects its generated wire
  // projection; C2 GA and Beta roots address one File/Skill identity authority;
  // C3 process B opens the exact durable directory committed by process A; C4
  // principal is absent, read-only, or admin; C5 Session/Event and
  // MemoryStore/Memory are committed beside File/Skill. Effects: E1 absent fails 401;
  // E2 read-only lists but cannot mutate (403 and no write); E3 both roots
  // observe the same ids before restart; E4 GA and the candidate Session/Memory
  // clients read every aggregate after restart; E5 the Beta-only archive
  // operation still reads immutable Version bytes; E6 deletion through GA is
  // immediately visible through Beta.
  // Decision rules: P1 !C4->E1; P2 reader->E2; P3 C1+C2+C3+admin->E3..E6.
  // This catches an accidental projection-specific repository, PEP bypass,
  // denied-write side effect, memory-only success, stale cache resurrection,
  // or route adapter that changes identity across restart.
  const storageDir = mkdtempSync(resolve(tmpdir(), 'awaken-managed-sdk-recovery-'));
  const port = await availablePort(Number(process.env.E2E_PORT));
  const upstream = await startUpstream('mcp');
  const servers = [];
  const environment = {
    ...deploymentEnv(storageDir, {
      identityMode: 'self-managed',
      iamWorkspaces: ['default'],
      controlSealKey: '01'.repeat(32),
    }),
    ...realServerEnv('mcp', upstream, { mode: 'management' }),
  };
  const clientFor = (baseURL, authToken) => new Anthropic({
    ...(authToken ? { authToken } : { apiKey: 'unauthorized' }), // awaken-allow: secret
    baseURL,
    fetch: candidateRecordingFetch,
    maxRetries: 0,
  });
  try {
    const a = spawnServer('management', port, environment);
    servers.push(a.server);
    await waitForPort(port, 900_000, a.server);
    const anonymous = clientFor(a.baseUrl);
    await assert.rejects(
      () => drain(anonymous.beta.files.list({ limit: 1 })),
      managedError(401, 'authentication_error'),
      'P1/E1 authentication precedes candidate Files reads',
    );
    await assert.rejects(
      () => drain(anonymous.beta.skills.list({ limit: 1 })),
      managedError(401, 'authentication_error'),
      'P1/E1 authentication precedes candidate Skills reads',
    );

    const adminToken = readFileSync(resolve(storageDir, 'admin-token'), 'utf8').trim();
    const minted = await fetch(`${a.baseUrl}/v1/config/iam/tokens`, {
      method: 'POST',
      headers: {
        authorization: `Bearer ${adminToken}`,
        'content-type': 'application/json',
      },
      body: JSON.stringify({ workspace_id: 'default', role: 'workspace_user' }),
    });
    const mintedText = await minted.text();
    assert.equal(minted.status, 201, `P1 mint read-only Resource principal: ${mintedText}`);
    const readerToken = JSON.parse(mintedText).token;
    assert.equal(typeof readerToken, 'string', 'P2 read-only principal credential');
    const reader = clientFor(a.baseUrl, readerToken);
    assert.deepEqual(await drain(reader.beta.files.list({ limit: 1 })), [], 'P2/E2 File read');
    assert.deepEqual(await drain(reader.beta.skills.list({ limit: 1 })), [], 'P2/E2 Skill read');
    const body = '---\nname: candidate-recovery\ndescription: restart proof\n---\nRecovered.';
    await assert.rejects(
      async () => reader.beta.files.upload({
        file: await toFile(Buffer.from('forbidden'), 'forbidden.txt'),
        ...(betaFiles.projection === 'ga' ? { expires_in_seconds: 3_600 } : {}),
      }),
      managedError(403, 'permission_error'),
      'P2/E2 read-only principal cannot mutate Files',
    );
    await assert.rejects(
      async () => reader.beta.skills.create({
        ...(betaSkills.projection === 'beta'
          ? { display_title: 'Candidate Recovery' }
          : { display_name: 'Candidate Recovery' }),
        files: [await toFile(Buffer.from(body), 'SKILL.md')],
      }),
      managedError(403, 'permission_error'),
      'P2/E2 read-only principal cannot mutate Skills',
    );
    assert.deepEqual(
      await drain(reader.beta.files.list({ limit: 1 })),
      [],
      'P2/E2 denied File write has no catalog effect',
    );
    assert.deepEqual(
      await drain(reader.beta.skills.list({ limit: 1 })),
      [],
      'P2/E2 denied Skill write has no store effect',
    );

    let client = clientFor(a.baseUrl, adminToken);
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
    });
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `candidate-recovery-${manifest.version}` }],
      }],
    });
    const acceptedId = receipt.data[0]?.id;
    assert.equal(typeof acceptedId, 'string', 'P3/E3 durable Session receipt');
    await waitForSessionEventReceipt(
      client,
      session.id,
      acceptedId,
      undefined,
      ({ delta }) => delta.some((event) => event.type === 'agent.message')
        && delta.some((event) => event.type === 'session.status_idle'),
      'P3/E3 candidate Session settles before restart',
      { pollMs: 10 },
    );
    const memoryStore = await client.beta.memoryStores.create({
      name: `candidate-recovery-${manifest.version}`,
    });
    const memory = await client.beta.memoryStores.memories.create(memoryStore.id, {
      path: '/candidate-recovery.md',
      content: manifest.version,
      view: 'full',
    });
    const file = await client.beta.files.upload({
      file: await toFile(Buffer.from(`recovery-${manifest.version}`), 'recovery.txt'),
      ...(betaFiles.projection === 'ga' ? { expires_in_seconds: 3_600 } : {}),
    });
    const skill = await client.beta.skills.create({
      ...(betaSkills.projection === 'beta'
        ? { display_title: 'Candidate Recovery' }
        : { display_name: 'Candidate Recovery' }),
      files: [await toFile(Buffer.from(body), 'SKILL.md')],
    });
    const versionReference = betaSkills.projection === 'beta'
      ? skill.latest_version
      : skill.latest_version_id;
    assert.equal((await client.files.retrieveMetadata(file.id)).id, file.id, 'P3/E3 File');
    assert.equal((await client.skills.retrieve(skill.id)).id, skill.id, 'P3/E3 Skill');

    await stopServer(a.server);
    servers.pop();
    const b = spawnServer('management', port, environment);
    servers.push(b.server);
    await waitForPort(port, 900_000, b.server);
    client = clientFor(b.baseUrl, adminToken);
    assert.equal((await client.beta.sessions.retrieve(session.id)).id, session.id, 'P3/E4 Session');
    const recoveredEvents = await drain(client.beta.sessions.events.list(session.id, { limit: 1 }));
    assert.ok(
      recoveredEvents.some((event) => event.id === acceptedId),
      'P3/E4 exact accepted Session Event survives restart',
    );
    assert.ok(
      recoveredEvents.some((event) => event.type === 'agent.message')
        && recoveredEvents.some((event) => event.type === 'session.status_idle'),
      'P3/E4 candidate decodes the recovered terminal Session history',
    );
    assert.equal(
      (await client.beta.memoryStores.memories.retrieve(memory.id, {
        memory_store_id: memoryStore.id,
      })).content,
      manifest.version,
      'P3/E4 Memory',
    );
    assert.equal((await client.files.retrieveMetadata(file.id)).id, file.id, 'P3/E4 File');
    assert.equal((await client.skills.retrieve(skill.id)).id, skill.id, 'P3/E4 Skill');
    const archive = await client.beta.skills.versions.download(versionReference, {
      skill_id: skill.id,
    });
    assert.match(await archive.text(), /Recovered\./u, 'P3/E5 immutable Version bytes');
    if (hasRuntime('tools/agent-toolset/skills.mjs')) {
      const helperWorkdir = mkdtempSync(resolve(tmpdir(), 'awaken-candidate-skill-helper-'));
      try {
        const cleanup = await setupSkills({
          client,
          session: {
            agent: { skills: [{ type: 'custom', skill_id: skill.id, version: 'latest' }] },
          },
          workdir: helperWorkdir,
        });
        const installed = join(helperWorkdir, 'skills', 'candidate-recovery', 'SKILL.md');
        assert.equal(existsSync(installed), true, 'P3/E5 setupSkills resolves latest after restart');
        assert.match(readFileSync(installed, 'utf8'), /Recovered\./u, 'P3/E5 helper archive bytes');
        await cleanup();
        assert.equal(existsSync(installed), false, 'P3/E5 helper cleanup removes its installation');
        completeBehaviorOwner('candidate.skill-setup-recovery');
      } finally {
        rmSync(helperWorkdir, { recursive: true, force: true });
      }
    }
    await client.files.delete(file.id);
    await client.skills.delete(skill.id);
    await client.beta.sessions.delete(session.id);
    await client.beta.memoryStores.memories.delete(memory.id, { memory_store_id: memoryStore.id });
    await client.beta.memoryStores.archive(memoryStore.id);
    await client.beta.memoryStores.delete(memoryStore.id);
    await assert.rejects(
      () => client.beta.files.retrieveMetadata(file.id),
      managedError(404, 'not_found_error'),
      'P3/E6 File deletion crosses roots',
    );
    await assert.rejects(
      () => client.beta.skills.retrieve(skill.id),
      managedError(404, 'not_found_error'),
      'P3/E6 Skill deletion crosses roots',
    );
    pass(`registry SDK ${manifest.version} preserves Beta-created Files/Skills across restart`);
  } finally {
    for (const server of servers) await stopServer(server);
    upstream.close();
    rmSync(storageDir, { recursive: true, force: true });
  }
}

await exerciseSdkCoreTransport();
await exerciseWorkspaceRequestRouting();
await exerciseStreamingBufferViews();
await exerciseSdkNodeRuntimeFacade();
await exerciseRefusalFallbackMiddleware();
await exerciseSdkHelperRuntime();

await withRealServer('echo', Number(process.env.E2E_PORT), async (baseURL) => {
  // Cause/effect graph: C0=Session Event send returns one exact durable
  // receipt; C1=the registry-latest generated Session and Memory
  // methods use their default beta/header behavior; C2=the echo topology owns
  // those resources. Effects are a decoded terminal Session stream plus Memory
  // CRUD round-trip. Decision rule R1: C0 && C1 && C2 => both resource families work;
  // any generated path/header/decoder drift fails at its official SDK call.
  // Constraints/invariant: the generated SDK methods and existing echo-owned
  // repositories are the only request/response paths; polling observes C0 and
  // cannot add a route, beta override, or second completion authority.
  const client = new Anthropic({
    apiKey: 'e2e-dummy',
    baseURL,
    fetch: candidateRecordingFetch,
  });
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
  });
  try {
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `latest-sdk-${manifest.version}` }],
      }],
    });
    const acceptedId = receipt.data[0]?.id;
    assert.equal(typeof acceptedId, 'string', 'R1 exact accepted User Event id');
    const { delta: events } = await waitForSessionEventReceipt(
      client,
      session.id,
      acceptedId,
      undefined,
      ({ delta }) => delta.some((event) => event.type === 'agent.message')
        && delta.some((event) => event.type === 'session.status_idle'),
      `registry SDK ${manifest.version} Session Run to settle`,
      { listParams: { limit: 1 } },
    );
    assert.ok(events.some((event) => event.type === 'agent.message'));
    assert.ok(events.some((event) => event.type === 'session.status_idle'));
    let replaySnapshot;
    let replaySawMessage = false;
    const replay = await client.beta.sessions.events.stream(session.id);
    for await (const event of replay) {
      replaySnapshot = accumulateManagedAgentsEvent(replaySnapshot, event);
      if (event.type === 'agent.message') replaySawMessage = true;
      if (replaySawMessage && event.type === 'session.status_idle') break;
    }
    assert.equal(
      replaySnapshot?.type,
      'agent.message',
      'R1 candidate SSE and accumulator reconstruct the canonical Agent message',
    );
    assert.ok(replaySnapshot.content.length > 0, 'R1 accumulated Agent message is non-empty');
    if (hasRuntime('lib/sessions/accumulate.mjs')) {
      completeBehaviorOwner('candidate.session-accumulator');
    }
  } finally {
    await client.beta.sessions.delete(session.id);
  }

  const store = await client.beta.memoryStores.create({ name: `latest-${manifest.version}` });
  const memory = await client.beta.memoryStores.memories.create(store.id, {
    path: '/latest.md',
    content: manifest.version,
    view: 'full',
  });
  assert.equal((await client.beta.memoryStores.memories.retrieve(memory.id, {
    memory_store_id: store.id,
  })).content, manifest.version);
  await client.beta.memoryStores.memories.delete(memory.id, { memory_store_id: store.id });
  await client.beta.memoryStores.archive(store.id);
  await client.beta.memoryStores.delete(store.id);

  await exerciseBetaFiles(client);
  await exerciseBetaSkills(client);

  // Cause/effect graph: C3=0.120 exposes GA Files and Skills outside `beta`;
  // C4=both project the existing FileCatalog/SkillStore; C5=GA Files expiry and
  // ids[] pagination, GA Model capabilities, and GA Skill source/latest-version
  // fields differ from beta.
  // Effects: E3 generated GA paths run without a beta header, E4 exact GA DTOs
  // decode, E5 the same created ids remain visible through their one repository.
  // Decision table: R3 C3+C4+C5 -> every GA Files/Skills method round-trips;
  // R4 C3+C4 -> GA Models list/retrieve decode from the existing inventory;
  // any accidental beta projection, missing expiry, or duplicate store fails.
  const file = await client.files.upload({
    file: await toFile(Buffer.from(manifest.version), 'latest.txt'),
    expires_in_seconds: 3600,
  });
  assert.equal(file.type, 'file', 'R3/E3');
  assert.equal(typeof file.expires_at, 'string', 'R3/E4 expiry');
  const files = await client.files.list({ ids: [file.id, MISSING_FILE_ID] });
  assert.deepEqual(files.data.map((item) => item.id), [file.id], 'R3/E5 ids[]');
  assert.equal((await client.files.retrieveMetadata(file.id)).id, file.id);
  await assert.rejects(
    () => client.files.download(file.id),
    managedError(400, 'invalid_request_error', /not downloadable/u),
    'R3 uploaded inputs remain non-downloadable through the latest GA client',
  );
  await client.files.delete(file.id);

  const models = [];
  for await (const model of client.models.list()) models.push(model);
  assert.ok(models.length > 0, 'R4 GA Models list is non-empty');
  assert.ok(models.every((model) => !Object.hasOwn(model, 'allowed_fallback_models')), 'R4 GA shape');
  assert.equal((await client.models.retrieve(models[0].id)).id, models[0].id, 'R4 retrieve');

  const skill = await client.skills.create({
    display_name: `Latest ${manifest.version}`,
    files: [await toFile(
      Buffer.from(`---\nname: latest-skill\ndescription: SDK ${manifest.version}\n---\n`),
      'latest-skill/SKILL.md',
    )],
  });
  assert.equal(skill.source.type, 'custom', 'R3/E4 source object');
  assert.equal(typeof skill.latest_version_id, 'string', 'R3/E4 version id');
  assert.equal((await client.skills.retrieve(skill.id)).id, skill.id);
  const skillPage = await client.skills.list({ source: 'custom' });
  assert.ok(skillPage.data.some((item) => item.id === skill.id), 'R3/E5 Skill list');
  const version = await client.skills.versions.create(skill.id, {
    files: [await toFile(
      Buffer.from(`---\nname: latest-skill\ndescription: SDK ${manifest.version} v2\n---\n`),
      'latest-skill/SKILL.md',
    )],
  });
  assert.equal(
    (await client.skills.versions.retrieve(version.id, { skill_id: skill.id })).id,
    version.id,
  );
  const versionPage = await client.skills.versions.list(skill.id);
  assert.ok(versionPage.data.some((item) => item.id === version.id), 'R3 Skill Version list');
  assert.equal(
    (await client.skills.versions.delete(skill.latest_version_id, { skill_id: skill.id })).type,
    'skill_version_deleted',
  );
  await client.skills.delete(skill.id);

  pass(
    `registry SDK ${manifest.version} runs Session, Memory, Beta/GA Models, Files, and Skills defaults`,
  );
});

// UserProfiles is Control-owned and intentionally absent from the echo runtime
// topology above. Exercise its existing management behavior owner without
// manufacturing a second route in that topology. Do not pass `betas`: the
// generated SDK method's own version header is the runtime behavior under review
// (0.117.1 emits the legacy selector; 0.120.0 emits the access_type selector).
await withScenarioServer('management', 'mcp', Number(process.env.E2E_PORT), async (baseURL) => {
  // Cause/effect graph: C3=UserProfiles remains Control-owned and absent from
  // echo; C4=the latest SDK supplies its own default version selector. Effect is
  // a create/retrieve round-trip decoded with access_type + relationship.
  // Decision rule R2: C3 && C4 => use the existing management topology without
  // explicit betas; route/header/shape drift fails rather than adding a canary-only route.
  // Constraints/invariant: Control remains the sole UserProfile owner and the
  // generated SDK remains the sole version-header owner; this canary may not
  // mirror either contract in the echo topology or in hand-authored transport.
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL });
  const profile = await client.beta.userProfiles.create({
    access_type: 'application',
    external_id: `latest-sdk-${manifest.version}`,
    ...(managedTypesV123
      ? { external_user_onboarded_at: '2026-09-03T01:02:03Z' }
      : {}),
  });
  assert.equal(profile.access_type, 'application');
  assert.equal(profile.relationship, undefined);
  if (managedTypesV123) {
    assert.equal(profile.external_user_onboarded_at, '2026-09-03T01:02:03Z');
  }
  const response = await client.beta.userProfiles.retrieve(profile.id).withResponse();
  assert.equal(response.data.id, profile.id);
  assert.equal(typeof response.workspace_id, 'string', '0.120 workspace response header');
  pass(`registry SDK ${manifest.version} runs UserProfile default beta`);
});

await exerciseGeneratedFileDownload();
await exerciseBetaGaRecovery();

// Causal/FMECA closure for candidate operation drift:
// C1 the extracted candidate inventory is the sole expected-operation source;
// C2 the exact candidate SDK emits each request; C3 a real Awaken handler
// returns a successful 2xx application response. Effect E1 every Files/Skills operation
// has a matching method/path/query/capability/Stainless/exact-version receipt. Authentication
// and authorization failures are deliberately excluded: 401/403 prove PEP
// ordering but not entry into the resource owner. Missing calls, a nearby route,
// lost selector, direct fetch, server fault, or newly changed operation therefore
// fails closed instead of producing source-only compatibility evidence.
const businessReceipts = candidateReceipts.filter(({ status }) => status !== 401 && status !== 403);
const coveredCandidateOperations = assertOwnerOperationReceipts(
  candidateResourceOperations,
  'candidate-files-skills',
  businessReceipts,
  manifest.version,
);
const coveredIds = new Set(candidateResourceOperations.map(({ sdkMethod }) => sdkMethod));
for (const { id } of candidateDelta.operations.changed) {
  assert.ok(coveredIds.has(id), `changed candidate operation lacks runtime owner: ${id}`);
}
const retainedBehaviorOwners = assertRetainedOfficialSdkBehaviorOwners(
  manifest.version, qualificationCatalog.qualifications, completedBehaviorOwners,
);

console.log(
  `SDK LATEST RUNTIME CANARY PASS: @anthropic-ai/sdk ${manifest.version}; `
  + `beta.files=${betaFiles.projection}, beta.skills=${betaSkills.projection}, `
  + `parseUnverified=${webhookProfile.parseUnverified}, `
  + 'typescript=pass, '
  + `resource_operations=${coveredCandidateOperations}, `
  + `operation_changes=${candidateDelta.operations.changed.length}, `
  + `declaration_changes=${candidateDelta.declarations.changed.length}, `
  + `runtime_changes=${candidateDelta.runtime.changed.length}, `
  + `completed_behavior_owners=${completedBehaviorOwners.size}, retained_behavior_owners=${retainedBehaviorOwners.length}.`,
);
