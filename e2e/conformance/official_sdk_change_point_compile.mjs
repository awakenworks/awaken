import { realpathSync } from 'node:fs';
import { resolve } from 'node:path';
import ts from 'typescript';
import { pathBelongsToRoot } from '../../packages/managed-sdk-oracle/src/package-source.mjs';

function filesFixture(projection) {
  if (projection === 'beta') {
    return `
  const fileMetadata = await client.beta.files.upload({ file: upload });
  const filename: string = fileMetadata.filename;
  const scope = fileMetadata.scope;
  void filename; void scope;
  await client.beta.files.retrieveMetadata(fileMetadata.id);
  await client.beta.files.list({ before_id: fileMetadata.id, limit: 1 });
  await client.beta.files.download(fileMetadata.id);
  await client.beta.files.delete(fileMetadata.id);`;
  }
  if (projection === 'ga') {
    return `
  const fileMetadata = await client.beta.files.upload({ file: upload, expires_in_seconds: 3_600 });
  const filename: string = fileMetadata.filename;
  const expiresAt: string | null | undefined = fileMetadata.expires_at;
  void filename; void expiresAt;
  await client.beta.files.retrieveMetadata(fileMetadata.id);
  await client.beta.files.list({ ids: [fileMetadata.id, 'file_missing'] });
  await client.beta.files.download(fileMetadata.id);
  await client.beta.files.delete(fileMetadata.id);`;
  }
  throw new Error(`unsupported Files projection ${JSON.stringify(projection)}`);
}

function skillsFixture(projection) {
  if (projection === 'beta') {
    return `
  const skill = await client.beta.skills.create({ display_title: 'Compile', files: [upload] });
  const displayTitle: string | null = skill.display_title;
  const firstVersion: string | null = skill.latest_version;
  void displayTitle; void firstVersion;
  await client.beta.skills.retrieve(skill.id);
  await client.beta.skills.list({ limit: 1 });
  const version = await client.beta.skills.versions.create(skill.id, { files: [upload] });
  const versionReference: string = version.version;
  await client.beta.skills.versions.retrieve(versionReference, { skill_id: skill.id });
  await client.beta.skills.versions.list(skill.id, { limit: 1 });
  await client.beta.skills.versions.download(versionReference, { skill_id: skill.id });
  await client.beta.skills.versions.delete(versionReference, { skill_id: skill.id });
  await client.beta.skills.delete(skill.id);`;
  }
  if (projection === 'ga') {
    return `
  const skill = await client.beta.skills.create({ display_name: 'Compile', files: [upload] });
  const displayName: string | null = skill.display_name;
  const sourceType: string = skill.source.type;
  const firstVersion: string | null = skill.latest_version_id;
  void displayName; void sourceType; void firstVersion;
  await client.beta.skills.retrieve(skill.id);
  await client.beta.skills.list({ source: 'custom', limit: 1 });
  const version = await client.beta.skills.versions.create(skill.id, { files: [upload] });
  const versionReference: string = version.id;
  await client.beta.skills.versions.retrieve(versionReference, { skill_id: skill.id });
  await client.beta.skills.versions.list(skill.id, { limit: 1 });
  await client.beta.skills.versions.download(versionReference, { skill_id: skill.id });
  await client.beta.skills.versions.delete(versionReference, { skill_id: skill.id });
  await client.beta.skills.delete(skill.id);`;
  }
  throw new Error(`unsupported Skills projection ${JSON.stringify(projection)}`);
}

function managedTypesFixture(enabled) {
  if (!enabled) return '';
  return `
  const fableModel: import('@anthropic-ai/sdk/resources/beta/agents/agents').BetaManagedAgentsModel =
    'claude-fable-5-1';
  const held: import('@anthropic-ai/sdk/resources/beta/dreams').BetaTargetStoreHeldError = {
    type: 'conflict_error',
    message: 'target store held',
  };
  const dreaming: import('@anthropic-ai/sdk/resources/beta/dreams').BetaDreamingError = held;
  type CandidateProfile = import('@anthropic-ai/sdk/resources/beta/user-profiles').BetaUserProfile;
  const relationshipIsAbsent:
    'relationship' extends keyof CandidateProfile ? never : true = true;
  const profile: CandidateProfile = {
    id: 'uprof_compile',
    created_at: '2026-09-03T00:00:00Z',
    metadata: {},
    trust_grants: {},
    type: 'user_profile',
    updated_at: '2026-09-03T00:00:00Z',
    external_user_onboarded_at: null,
  };
  const createProfile:
    import('@anthropic-ai/sdk/resources/beta/user-profiles').UserProfileCreateParams = {
      external_user_onboarded_at: '2026-09-03T00:00:00Z',
    };
  const updateProfile:
    import('@anthropic-ai/sdk/resources/beta/user-profiles').UserProfileUpdateParams = {
      external_user_onboarded_at: '2026-09-03T00:00:00Z',
    };
  const listProfiles:
    import('@anthropic-ai/sdk/resources/beta/user-profiles').UserProfileListParams = {
      order: 'asc', order_by: 'name',
    };
  type DeploymentRun =
    import('@anthropic-ai/sdk/resources/beta/deployment-runs').BetaManagedAgentsDeploymentRun;
  type Deployment =
    import('@anthropic-ai/sdk/resources/beta/deployments').BetaManagedAgentsDeployment;
  type Memory =
    import('@anthropic-ai/sdk/resources/beta/memory-stores/memories').BetaManagedAgentsMemory;
  type SessionEvent =
    import('@anthropic-ai/sdk/resources/beta/sessions/events').BetaManagedAgentsSessionEvent;
  type Session =
    import('@anthropic-ai/sdk/resources/beta/sessions/sessions').BetaManagedAgentsSession;
  const declarationOwners:
    [DeploymentRun, Deployment, Memory, SessionEvent, Session] | undefined = undefined;
  void fableModel; void dreaming; void relationshipIsAbsent; void profile;
  void createProfile; void updateProfile; void listProfiles; void declarationOwners;`;
}

function managedTypesV125Fixture(enabled) {
  if (!enabled) return '';
  // 0.125 change points: the `auto` permission policy on Agent tool configs,
  // the per-call `evaluation` on tool-use events, external user details on
  // User Profiles, and an optional GitHub authorization token.
  return `
  const autoPolicy: import('@anthropic-ai/sdk/resources/beta/agents/agents').BetaManagedAgentsAutoPolicy =
    { type: 'auto' };
  const autoToolset: import('@anthropic-ai/sdk/resources/beta/agents/agents').BetaManagedAgentsAgentToolset20260401Params = {
    type: 'agent_toolset_20260401',
    default_config: { enabled: true, permission_policy: autoPolicy },
  };
  const evaluation: import('@anthropic-ai/sdk/resources/beta/sessions/events').BetaManagedAgentsAgentToolEvaluation = {
    type: 'auto',
    evaluated_permission: { type: 'ask', reason_code: 'compile_only' },
  };
  const evaluatedToolUse: import('@anthropic-ai/sdk/resources/beta/sessions/events').BetaManagedAgentsAgentToolUseEvent = {
    id: 'event_compile', input: {}, name: 'read', processed_at: '2026-09-13T00:00:00Z',
    type: 'agent.tool_use', evaluated_permission: 'ask', evaluation,
  };
  const evaluatedMcpToolUse: import('@anthropic-ai/sdk/resources/beta/sessions/events').BetaManagedAgentsAgentMCPToolUseEvent = {
    id: 'event_compile_mcp', input: {}, name: 'add', mcp_server_name: 'calc',
    processed_at: '2026-09-13T00:00:00Z', type: 'agent.mcp_tool_use', evaluation,
  };
  const externalUser: import('@anthropic-ai/sdk/resources/beta/user-profiles').BetaUserProfileExternalUserDetailsParams = {
    account_status: 'active', country: 'DE', entity_type: 'business', reference_id: 'ref_compile',
  };
  const createDetailedProfile:
    import('@anthropic-ai/sdk/resources/beta/user-profiles').UserProfileCreateParams = {
      external_user_details: externalUser,
    };
  const publicRepository: import('@anthropic-ai/sdk/resources/beta/sessions/sessions').BetaManagedAgentsGitHubRepositoryResourceParams = {
    type: 'github_repository', url: 'https://github.com/awaken/public',
  };
  const authorizationIsOptional:
    undefined extends import('@anthropic-ai/sdk/resources/beta/sessions/sessions').BetaManagedAgentsGitHubRepositoryResourceParams['authorization_token'] ? true : never = true;
  void autoToolset; void evaluatedToolUse; void evaluatedMcpToolUse; void createDetailedProfile;
  void publicRepository; void authorizationIsOptional;`;
}
function workspaceRoutingFixture(enabled) {
  if (!enabled) return '';
  return `
  const workspace_id = 'workspace_compile';
  await client.beta.agents.list({ workspace_id });
  await client.beta.agents.versions.list('agent_compile', { workspace_id });
  await client.beta.deploymentRuns.list({ workspace_id });
  await client.beta.deployments.list({ workspace_id });
  await client.beta.dreams.list({ workspace_id });
  await client.beta.environments.list({ workspace_id });
  await client.beta.environments.work.stats('env_compile', { workspace_id });
  await client.beta.files.list({ workspace_id });
  await client.beta.memoryStores.memories.list('memstore_compile', { workspace_id });
  await client.beta.memoryStores.list({ workspace_id });
  await client.beta.memoryStores.memoryVersions.list('memstore_compile', { workspace_id });
  await client.beta.models.list({ workspace_id });
  await client.beta.sessions.events.list('session_compile', { workspace_id });
  await client.beta.sessions.resources.list('session_compile', { workspace_id });
  await client.beta.sessions.list({ workspace_id });
  await client.beta.sessions.threads.events.list('thread_compile', {
    session_id: 'session_compile', workspace_id,
  });
  await client.beta.sessions.threads.list('session_compile', { workspace_id });
  await client.beta.skills.list({ workspace_id });
  await client.beta.skills.versions.list('skill_compile', { workspace_id });
  await client.beta.tunnels.certificates.list('tunnel_compile', { workspace_id });
  await client.beta.tunnels.list({ workspace_id });
  await client.beta.vaults.credentials.list('vault_compile', { workspace_id });
  await client.beta.vaults.list({ workspace_id });
  await client.files.list({ workspace_id });
  await client.models.list({ workspace_id });
  await client.skills.list({ workspace_id });
  await client.skills.versions.list('skill_compile', { workspace_id });`;
}

function managedTypesV128Fixture(enabled) {
  if (!enabled) return '';
  // 0.128 closes one shared permission vocabulary across ordinary and MCP
  // tool events, re-exported from the Session namespace. Both equality checks
  // reject a wider/narrower event-specific enum; no cast or type escape is used.
  return `
  {
  type SameType<A, B> = (<T>() => T extends A ? 1 : 2) extends
    (<T>() => T extends B ? 1 : 2) ? true : false;
  type EvaluatedPermission = import('@anthropic-ai/sdk/resources/beta/sessions/events').BetaManagedAgentsAgentEvaluatedPermission;
  type SessionPermission = import('@anthropic-ai/sdk/resources/beta/sessions/sessions').Sessions.BetaManagedAgentsAgentEvaluatedPermission;
  type ToolPermission = NonNullable<import('@anthropic-ai/sdk/resources/beta/sessions/events').BetaManagedAgentsAgentToolUseEvent['evaluated_permission']>;
  type McpPermission = NonNullable<import('@anthropic-ai/sdk/resources/beta/sessions/events').BetaManagedAgentsAgentMCPToolUseEvent['evaluated_permission']>;
  const closedPermission: SameType<EvaluatedPermission, 'allow' | 'ask' | 'deny'> = true;
  const sessionPermission: SameType<SessionPermission, EvaluatedPermission> = true;
  const toolPermission: SameType<ToolPermission, EvaluatedPermission> = true;
  const mcpPermission: SameType<McpPermission, EvaluatedPermission> = true;
  const newModel: import('@anthropic-ai/sdk/resources/beta/agents/agents').BetaManagedAgentsModel = 'claude-opus-5-5';
  type Dream = import('@anthropic-ai/sdk/resources/beta/dreams').BetaDream;
  type Memory = import('@anthropic-ai/sdk/resources/beta/memory-stores/memories').BetaManagedAgentsMemory;
  type MemoryVersion = import('@anthropic-ai/sdk/resources/beta/memory-stores/memory-versions').BetaManagedAgentsMemoryVersion;
  type Resource = import('@anthropic-ai/sdk/resources/beta/sessions/resources').BetaManagedAgentsSessionResource;
  type Thread = import('@anthropic-ai/sdk/resources/beta/sessions/threads/threads').BetaManagedAgentsSessionThread;
  type ThreadEvent = import('@anthropic-ai/sdk/resources/beta/sessions/threads/events').EventListParams;
  type UserProfile = import('@anthropic-ai/sdk/resources/beta/user-profiles').BetaUserProfile;
  const documentedOwners: [Dream, Memory, MemoryVersion, Resource, Thread, ThreadEvent, UserProfile] | undefined = undefined;
  void closedPermission; void sessionPermission; void toolPermission; void mcpPermission;
  void newModel; void documentedOwners;
  }`;
}

export function officialSdkChangePointFixture({
  filesProjection,
  skillsProjection,
  parseUnverified,
  resolveSkillVersion,
  managedTypesV123 = false,
  managedTypesV125 = false,
  managedTypesV127 = false,
  managedTypesV128 = false,
  workspaceRouting = false,
}) {
  return `// Generated in memory by official_sdk_change_point_compile.mjs.
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { accumulateManagedAgentsEvent } from '@anthropic-ai/sdk/lib/sessions/accumulate';
import {
  betaAgentToolset20260401,
  setupSkills,
  ${resolveSkillVersion ? 'resolveSkillVersion,' : ''}
} from '@anthropic-ai/sdk/tools/agent-toolset/node';
type AgentToolsetModule = typeof import('@anthropic-ai/sdk/tools/agent-toolset/node');

const client = new Anthropic({ apiKey: 'compile-only' }); // awaken-allow: secret

async function exerciseChangePoints() {
  ${resolveSkillVersion ? `const resolveVersion: typeof resolveSkillVersion = resolveSkillVersion;
  void resolveVersion;` : `const noRetiredResolveHelper:
    'resolveSkillVersion' extends keyof AgentToolsetModule ? never : true = true;
  void noRetiredResolveHelper;`}
  const upload = await toFile(new Uint8Array([1]), 'fixture.txt');
  const tools = betaAgentToolset20260401({ workdir: '/tmp' });
  const toolNames: string[] = tools.map(({ name }) => name);
  await setupSkills({ workdir: '/tmp' });
  const accumulate: typeof accumulateManagedAgentsEvent = accumulateManagedAgentsEvent;
  void toolNames; void accumulate;
${filesFixture(filesProjection)}
${skillsFixture(skillsProjection)}
${managedTypesFixture(managedTypesV123)}
${managedTypesV125Fixture(managedTypesV125)}
${managedTypesV127 ? `
  const compaction: import('@anthropic-ai/sdk/resources/beta/models').BetaCompactionCapability = {
    supported: true, summarize: { supported: true },
  };
  const model = await client.beta.models.retrieve('model-compile');
  const optionalCompaction: typeof compaction | null | undefined = model.capabilities?.compaction;
  void optionalCompaction;
  await client.beta.userProfiles.create({ workspace_id: 'workspace-compile' });
  await client.beta.userProfiles.retrieve('profile-compile', { workspace_id: 'workspace-compile' });
  await client.beta.userProfiles.update('profile-compile', { workspace_id: 'workspace-compile' });
  await client.beta.userProfiles.list({ workspace_id: 'workspace-compile' });
  await client.beta.userProfiles.createEnrollmentURL('profile-compile', { workspace_id: 'workspace-compile' });
` : ''}
${workspaceRoutingFixture(workspaceRouting)}
${managedTypesV128Fixture(managedTypesV128)}
  client.beta.webhooks.unwrap('{}', { headers: {
    'webhook-id': 'event_compile',
    'webhook-timestamp': '0',
    'webhook-signature': 'v1,compile',
  } });
  ${parseUnverified ? "client.beta.webhooks.parseUnverified('{}');" : ''}
}

void exerciseChangePoints();
`;
}

export function compileOfficialSdkChangePoints(packageRoot, profile) {
  // Cause/effect graph: C1 official generated request signatures select one
  // Files/Skills projection; C2 the SDK exposes a reviewed Webhook helper set;
  // C3 an exact, externally provisioned package root owns declaration lookup;
  // C4 request-scoped Workspace routing is present or absent as one versioned
  // capability across the complete changed Resource declaration set.
  // Effect: E1 every changed method accepts its intended strongly typed input
  // and exposes the required response fields. Decision rule T1 C1+C2+C3->E1;
  // any missing/renamed field, parameter, method, or module fails compilation.
  // W1 C4=>every changed Resource family accepts the same typed Workspace
  // selector; W2 !C4=>no candidate-only call is emitted.
  // Constraint: the fixture contains no any, cast, ts-ignore, or handwritten
  // declaration and exists only in the compiler host, never in product code.
  const source = officialSdkChangePointFixture(profile);
  const forbidden = [/\bany\b/u, /\sas\s/u, /@ts-/u];
  for (const pattern of forbidden) {
    if (pattern.test(source)) throw new Error(`candidate fixture contains forbidden ${pattern}`);
  }

  const packagePrefix = resolve(packageRoot, '../../..');
  const virtualFile = resolve(packagePrefix, 'awaken-managed-candidate-canary.mts');
  const options = {
    allowSyntheticDefaultImports: true,
    esModuleInterop: true,
    lib: ['lib.es2022.d.ts', 'lib.dom.d.ts', 'lib.dom.iterable.d.ts'],
    module: ts.ModuleKind.NodeNext,
    moduleResolution: ts.ModuleResolutionKind.NodeNext,
    noEmit: true,
    skipLibCheck: true,
    strict: true,
    target: ts.ScriptTarget.ES2022,
    types: [],
  };
  const host = ts.createCompilerHost(options);
  const defaultGetSourceFile = host.getSourceFile.bind(host);
  const defaultFileExists = host.fileExists.bind(host);
  const defaultReadFile = host.readFile.bind(host);
  host.fileExists = (filename) => filename === virtualFile || defaultFileExists(filename);
  host.readFile = (filename) => filename === virtualFile ? source : defaultReadFile(filename);
  host.getSourceFile = (filename, languageVersion, onError, shouldCreateNewSourceFile) => (
    filename === virtualFile
      ? ts.createSourceFile(filename, source, languageVersion, true, ts.ScriptKind.TS)
      : defaultGetSourceFile(filename, languageVersion, onError, shouldCreateNewSourceFile)
  );
  const resolvedSdk = ts.resolveModuleName('@anthropic-ai/sdk', virtualFile, options, host)
    .resolvedModule;
  if (!resolvedSdk) {
    throw new Error(`candidate TypeScript cannot resolve @anthropic-ai/sdk from ${packageRoot}`);
  }
  const canonicalRoot = realpathSync(packageRoot);
  const canonicalModule = realpathSync(resolvedSdk.resolvedFileName);
  if (!pathBelongsToRoot(canonicalRoot, canonicalModule)) {
    throw new Error(
      `candidate TypeScript resolved @anthropic-ai/sdk outside ${canonicalRoot}: ${canonicalModule}`,
    );
  }
  const program = ts.createProgram([virtualFile], options, host);
  const diagnostics = ts.getPreEmitDiagnostics(program);
  if (diagnostics.length > 0) {
    throw new Error(ts.formatDiagnosticsWithColorAndContext(diagnostics, {
      getCanonicalFileName: (filename) => filename,
      getCurrentDirectory: () => packagePrefix,
      getNewLine: () => '\n',
    }));
  }
}
