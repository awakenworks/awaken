// Machine-owned method-level ownership for the official TypeScript Managed SDK.
// Behavioral rule ids live beside the executable tests that implement them; this
// index deliberately does not synthesize labels that are not executable proof.

const group = (sdkRoot, prefix, route, owner, methods, overrides = {}, sdkHelpers = {}) =>
  methods.map((method) => {
    const relativeMethod = `${prefix}.${method}`;
    const sdkMethod = sdkRoot === 'beta' ? `beta.${relativeMethod}` : relativeMethod;
    return {
      sdkMethod,
      sdkRoot,
      relativeMethod,
      route,
      owner: overrides[method] ?? owner,
      ...(sdkHelpers[method] ? { sdkHelper: sdkHelpers[method] } : {}),
    };
  });

const beta = (...args) => group('beta', ...args);
const ga = (...args) => group('ga', ...args);

export const MANAGED_TS_METHOD_MANIFEST = [
  ...beta('models', '/v1/models', 'management_files_models_e2e.mjs', ['retrieve', 'list']),
  ...beta('agents', '/v1/agents', 'management_agents_e2e.mjs',
    ['create', 'retrieve', 'update', 'list', 'archive']),
  ...beta('agents.versions', '/v1/agents/{agent_id}/versions', 'management_agents_e2e.mjs', ['list']),
  ...beta('environments', '/v1/environments', 'management_environments_e2e.mjs',
    ['create', 'retrieve', 'update', 'list', 'delete', 'archive']),
  ...beta('environments.work', '/v1/environments/{environment_id}/work',
    'management_environment_work_depth_e2e.mjs',
    ['retrieve', 'update', 'list', 'ack', 'heartbeat', 'poll', 'stats', 'stop', 'poller', 'worker'],
    {
      poller: 'management_official_worker_e2e.mjs',
      worker: 'management_environment_worker_full_e2e.mjs',
    }),
  ...beta('sessions', '/v1/sessions', 'management_sessions_family_e2e.mjs',
    ['create', 'retrieve', 'update', 'list', 'delete', 'archive']),
  ...beta('sessions.events', '/v1/sessions/{session_id}/events', 'managed_e2e.mjs',
    ['list', 'send', 'stream', 'toolRunner'],
    { toolRunner: 'managed_session_tool_runner_matrix_e2e.mjs' },
    { list: 'harness.mjs#waitForSessionEventReceipt' }),
  ...beta('sessions.resources', '/v1/sessions/{session_id}/resources',
    'managed_resource_lifecycle_e2e.mjs', ['retrieve', 'update', 'list', 'delete', 'add'],
    { update: 'managed_session_resource_rotation_e2e.mjs' }),
  ...beta('sessions.threads', '/v1/sessions/{session_id}/threads',
    'management_session_threads_e2e.mjs', ['retrieve', 'list', 'archive']),
  ...beta('sessions.threads.events', '/v1/sessions/{session_id}/threads/{thread_id}/events',
    'management_session_threads_e2e.mjs', ['list', 'stream']),
  ...beta('deployments', '/v1/deployments', 'management_deployments_e2e.mjs',
    ['create', 'retrieve', 'update', 'list', 'archive', 'pause', 'run', 'unpause']),
  ...beta('deploymentRuns', '/v1/deployment_runs', 'management_deployments_e2e.mjs',
    ['retrieve', 'list']),
  ...beta('vaults', '/v1/vaults', 'management_vaults_family_e2e.mjs',
    ['create', 'retrieve', 'update', 'list', 'delete', 'archive'],
    { retrieve: 'management_vaults_e2e.mjs', delete: 'management_vaults_e2e.mjs' }),
  ...beta('vaults.credentials', '/v1/vaults/{vault_id}/credentials',
    'management_vaults_family_e2e.mjs',
    ['create', 'retrieve', 'update', 'list', 'delete', 'archive', 'mcpOAuthValidate'],
    { mcpOAuthValidate: 'management_vaults_e2e.mjs' }),
  ...beta('memoryStores', '/v1/memory_stores', 'management_memory_stores_e2e.mjs',
    ['create', 'retrieve', 'update', 'list', 'delete', 'archive']),
  ...beta('memoryStores.memories', '/v1/memory_stores/{memory_store_id}/memories',
    'management_memory_stores_e2e.mjs', ['create', 'retrieve', 'update', 'list', 'delete']),
  ...beta('memoryStores.memoryVersions', '/v1/memory_stores/{memory_store_id}/memory_versions',
    'management_memory_stores_e2e.mjs', ['retrieve', 'list', 'redact']),
  ...beta('files', '/v1/files', 'management_files_models_e2e.mjs',
    ['list', 'delete', 'download', 'retrieveMetadata', 'upload'],
    {
      list: 'managed_resources_api_e2e.mjs',
      download: 'managed_resources_api_e2e.mjs',
      retrieveMetadata: 'managed_full_lifecycle_e2e.mjs',
    }),
  ...beta('skills', '/v1/skills', 'management_skills_e2e.mjs',
    ['create', 'retrieve', 'list', 'delete']),
  ...beta('skills.versions', '/v1/skills/{skill_id}/versions', 'management_skills_e2e.mjs',
    ['create', 'retrieve', 'list', 'delete', 'download']),
  ...beta('webhooks', 'offline-standard-webhooks', 'managed_webhooks_official_sdk_e2e.mjs', ['unwrap']),
  ...beta('userProfiles', '/v1/user_profiles', 'management_user_profiles_e2e.mjs',
    ['create', 'retrieve', 'update', 'list', 'createEnrollmentURL']),
  ...beta('dreams', '/v1/dreams', 'managed_dream_e2e.ts',
    ['create', 'retrieve', 'list', 'archive', 'cancel']),
  ...beta('tunnels', '/v1/tunnels', 'management_tunnels_contract_e2e.mjs',
    ['create', 'retrieve', 'list', 'archive', 'revealToken', 'rotateToken']),
  ...beta('tunnels.certificates', '/v1/tunnels/{tunnel_id}/certificates',
    'management_tunnels_contract_e2e.mjs', ['create', 'retrieve', 'list', 'archive']),
  ...ga('models', '/v1/models', 'management_files_models_e2e.mjs', ['retrieve', 'list']),
  ...ga('files', '/v1/files', 'management_files_models_e2e.mjs',
    ['list', 'delete', 'download', 'retrieveMetadata', 'upload'],
    { download: 'managed_namespace_session_environment_e2e.mjs' }),
  ...ga('skills', '/v1/skills', 'management_skills_e2e.mjs',
    ['create', 'retrieve', 'list', 'delete']),
  ...ga('skills.versions', '/v1/skills/{skill_id}/versions', 'management_skills_e2e.mjs',
    ['create', 'retrieve', 'list', 'delete']),
].sort((a, b) => a.sdkMethod.localeCompare(b.sdkMethod));
