// Executable E2E ownership for the official TypeScript Managed SDK.
//
// HTTP operation identity, method and route come exclusively from the generated
// current-SDK oracle. This file owns only the orthogonal question "which real
// process scenario proves this operation?" and the four generated SDK helpers
// that do not correspond to an HTTP operation. Keeping those authorities
// separate prevents a hand-maintained method inventory from certifying a stale
// SDK while still making every behavior owner reviewable.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const coverage = JSON.parse(readFileSync(resolve(
  import.meta.dirname,
  '../../contracts/anthropic-managed/operation-coverage.generated.json',
), 'utf8'));

const OWNER_PREFIXES = new Map([
  ['beta.agents.', 'management_agents_e2e.mjs'],
  ['beta.deploymentRuns.', 'management_deployments_e2e.mjs'],
  ['beta.deployments.', 'management_deployments_e2e.mjs'],
  ['beta.dreams.', 'managed_dream_e2e.ts'],
  ['beta.environments.work.', 'management_environment_work_depth_e2e.mjs'],
  ['beta.environments.', 'management_environments_e2e.mjs'],
  ['beta.files.', 'management_files_models_e2e.mjs'],
  ['beta.memoryStores.', 'management_memory_stores_e2e.mjs'],
  ['beta.models.', 'management_files_models_e2e.mjs'],
  ['beta.sessions.resources.', 'managed_resource_lifecycle_e2e.mjs'],
  ['beta.sessions.threads.', 'management_session_threads_e2e.mjs'],
  ['beta.sessions.events.', 'managed_e2e.mjs'],
  ['beta.sessions.', 'management_sessions_family_e2e.mjs'],
  ['beta.skills.', 'management_skills_e2e.mjs'],
  ['beta.tunnels.', 'management_tunnels_contract_e2e.mjs'],
  ['beta.userProfiles.', 'management_user_profiles_e2e.mjs'],
  ['beta.vaults.', 'management_vaults_family_e2e.mjs'],
  ['files.', 'management_files_models_e2e.mjs'],
  ['models.', 'management_files_models_e2e.mjs'],
  ['skills.', 'management_skills_e2e.mjs'],
]);

const OWNER_OVERRIDES = new Map([
  ['beta.files.download', 'managed_resources_api_e2e.mjs'],
  ['beta.files.list', 'managed_resources_api_e2e.mjs'],
  ['beta.files.retrieveMetadata', 'managed_full_lifecycle_e2e.mjs'],
  ['beta.sessions.resources.update', 'managed_session_resource_rotation_e2e.mjs'],
  ['beta.vaults.credentials.mcpOAuthValidate', 'management_vaults_e2e.mjs'],
  ['beta.vaults.delete', 'management_vaults_e2e.mjs'],
  ['beta.vaults.retrieve', 'management_vaults_e2e.mjs'],
]);

const SDK_HELPERS = [
  {
    sdkMethod: 'beta.environments.work.poller',
    sdkRoot: 'beta',
    relativeMethod: 'environments.work.poller',
    route: 'generated-worker-helper',
    owner: 'management_official_worker_e2e.mjs',
  },
  {
    sdkMethod: 'beta.environments.work.worker',
    sdkRoot: 'beta',
    relativeMethod: 'environments.work.worker',
    route: 'generated-worker-helper',
    owner: 'management_official_worker_e2e.mjs',
  },
  {
    sdkMethod: 'beta.sessions.events.toolRunner',
    sdkRoot: 'beta',
    relativeMethod: 'sessions.events.toolRunner',
    route: 'generated-tool-runner-helper',
    owner: 'managed_session_tool_runner_matrix_e2e.mjs',
  },
  {
    sdkMethod: 'beta.webhooks.unwrap',
    sdkRoot: 'beta',
    relativeMethod: 'webhooks.unwrap',
    route: 'offline-standard-webhooks',
    owner: 'managed_webhooks_official_sdk_e2e.mjs',
  },
];

const SDK_HELPER_CALLS = new Map([
  ['beta.sessions.events.list', 'harness.mjs#waitForSessionEventReceipt'],
]);

function behaviorOwner(operationID) {
  const override = OWNER_OVERRIDES.get(operationID);
  if (override) return override;
  const matches = [...OWNER_PREFIXES]
    .filter(([prefix]) => operationID.startsWith(prefix))
    .sort(([left], [right]) => right.length - left.length);
  if (matches.length === 0) throw new Error(`${operationID}: no real-process behavior owner`);
  if (matches.length > 1 && matches[0][0].length === matches[1][0].length) {
    throw new Error(`${operationID}: ambiguous real-process behavior owner`);
  }
  return matches[0][1];
}

const operations = coverage.operations
  .filter(({ id }) => !id.startsWith('documented.'))
  .map((operation) => ({
    sdkMethod: operation.id,
    sdkRoot: operation.id.startsWith('beta.') ? 'beta' : 'ga',
    relativeMethod: operation.id.replace(/^beta\./u, ''),
    route: operation.path,
    method: operation.method,
    betas: operation.betas,
    ...(operation.transport_query ? { transportQuery: operation.transport_query } : {}),
    owner: behaviorOwner(operation.id),
    ...(SDK_HELPER_CALLS.has(operation.id)
      ? { sdkHelper: SDK_HELPER_CALLS.get(operation.id) }
      : {}),
  }));

export const MANAGED_TS_METHOD_MANIFEST = [...operations, ...SDK_HELPERS]
  .sort((left, right) => left.sdkMethod.localeCompare(right.sdkMethod));
