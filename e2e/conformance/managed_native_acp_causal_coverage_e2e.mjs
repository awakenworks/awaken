// Offline completeness gate for the Native / ACP / A2A Managed Agents cause graph.
// Behavioral assertions stay in their owning suites; this manifest prevents a
// feature family, backend partition, or negative partition from disappearing
// while the aggregate compatibility report still looks green.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ACP_RUNTIME_IDS } from '../acp_runtime_profiles.mjs';

const E2E = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const ROOT = path.resolve(E2E, '..');
const evidence = (file, token) => ({ file, token });
const acpPartitions = (file, token) => Object.fromEntries(
  ACP_RUNTIME_IDS.map((runtime) => [`acp:${runtime}`, evidence(file, token)]),
);
const PARTITIONS = Object.freeze([
  'native',
  ...ACP_RUNTIME_IDS.map((runtime) => `acp:${runtime}`),
  'a2a',
  'negative',
]);

// Test design M1 for the static completeness manifest.
// Causes: C1=the canonical catalog supplies the ordered ACP identities; C2=a feature
// group has shared or runtime-specific evidence; C3=Native/A2A/negative remain
// separate protocol partitions. Effects: E1=all catalog-derived columns resolve
// to a real file+token; E2=removing any catalog identity or evidence fails.
// Constraint: a shared evidence source means a shared production mechanism,
// never that the opt-in real CLI lane was executed. Decision rules:
// M1-A(C1+C2+C3)->E1; M1-B(missing cell or token)->E2.

const groups = [
  {
    id: 'session-events',
    native: evidence('e2e/acp_e2e.mjs', 'a native session on the same server'),
    ...acpPartitions('e2e/managed_native_acp_runtime_matrix_e2e.mjs', 'Test design D1'),
    a2a: evidence('e2e/cross_protocol_a2a_continuity_e2e.mjs', 'three-wire continuity'),
    negative: evidence('e2e/acp_e2e.mjs', 'Driver-error paths'),
  },
  {
    id: 'builtin-tools-and-spill',
    native: evidence('crates/runtime/awaken-ext-builtin-tools/src/hand.rs', 'Bash state cannot cross Sessions'),
    ...acpPartitions('e2e/managed_native_acp_runtime_matrix_e2e.mjs', 'ACP completed tool call/result pair'),
    a2a: evidence('e2e/a2a_hitl_decision_e2e.mjs', 'structured allow resumes the awaiting tool'),
    negative: evidence(
      'crates/runtime/awaken-ext-builtin-tools/tests/hand.rs',
      'G3 untrusted absolute pattern',
    ),
  },
  {
    id: 'background-tool-execution',
    native: evidence(
      'crates/runtime/awaken-runtime/tests/background_task_state.rs',
      'ids.contains("run_in_background")',
    ),
    ...acpPartitions(
      'crates/control/awaken-agent-config/src/compile.rs',
      'R2 ACP + background_task',
    ),
    a2a: evidence(
      'crates/control/awaken-agent-config/src/compile.rs',
      'R3 A2A + background_task',
    ),
    negative: evidence(
      'crates/runtime/awaken-ext-background-task/tests/plugin_state.rs',
      'model cannot select an',
    ),
  },
  {
    id: 'resources-memory-and-files',
    native: evidence('e2e/managed_resources_e2e.mjs', 'managed_memory_real_eval_native'),
    ...acpPartitions('e2e/acp_runtime_memory_matrix_e2e.mjs', 'managed_memory_real_eval_acp_runtime_matrix'),
    a2a: evidence('e2e/multi_protocol_environment_resource_matrix_e2e.mjs', "['ai-sdk', 'ag-ui', 'a2a']"),
    negative: evidence('e2e/managed_memory_extraction_durable_e2e.mjs', 'read_only'),
  },
  {
    id: 'skills',
    native: evidence('e2e/managed_skills_e2e.mjs', 'skill'),
    ...acpPartitions('e2e/managed_namespace_session_environment_e2e.mjs', 'NAMESPACE-SKILL-OK'),
    // An outbound A2A agent owns its remote skills and cannot consume a local
    // Session mount. Compatibility here is the explicit fail-closed boundary.
    a2a: evidence('crates/server/awaken-runtime-host/src/host/tests.rs', 'R2 local input was accepted'),
    negative: evidence('e2e/managed_skill_store_durable_e2e.mjs', 'missing `files`'),
  },
  {
    id: 'mcp',
    native: evidence('e2e/management_mcp_e2e.mjs', 'always_ask'),
    ...acpPartitions('e2e/acp_managed_mcp_e2e.mjs', 'projects only replacement'),
    a2a: evidence('crates/server/awaken-runtime-host/src/host/tests.rs', 'A2A-only IO context has no Environment and no Hand'),
    negative: evidence('crates/server/awaken-runtime-host/src/host/tests.rs', 'mcp_client_refresh_unsupported'),
  },
  {
    id: 'multiagent',
    native: evidence('e2e/managed_delegation_e2e.mjs', 'Native `researcher`'),
    ...acpPartitions('e2e/managed_native_acp_runtime_matrix_e2e.mjs', 'Test design G1'),
    a2a: evidence('e2e/managed_remote_delegation_e2e.mjs', 'remote A2A delegation round-tripped'),
    negative: evidence('crates/server/awaken-runtime-host/src/host/session/content_delivery.rs', '!published_backend_is_acp'),
  },
  {
    id: 'outcomes',
    native: evidence('e2e/managed_outcome_runtime_matrix_e2e.ts', 'native'),
    ...acpPartitions('e2e/managed_outcome_runtime_matrix_e2e.ts', 'acp'),
    a2a: evidence('e2e/auxiliary_windows_causal_graph_e2e.mjs', "id: 'CG-A1'"),
    negative: evidence('e2e/managed_outcome_recovery_e2e.ts', 'recovery'),
  },
  {
    id: 'self-hosted-worker-and-recovery',
    native: evidence('e2e/management_self_hosted_worker_e2e.mjs', 'claims a session'),
    ...acpPartitions('crates/bin/awaken-worker/tests/worker_node_lifecycle.rs', 'deployment.acp'),
    a2a: evidence('e2e/remote_attempt_lifecycle_e2e.ts', 'backend_ref round-trips'),
    negative: evidence('e2e/management_environments_e2e.mjs', 'reclaim_older_than_ms'),
  },
  {
    id: 'manual-and-scheduled-deployment',
    native: evidence('e2e/management_deployment_schedule_e2e.mjs', 'manual runs'),
    ...acpPartitions('crates/server/awaken-protocol-managed/tests/session_resources.rs', 'acp:claude'),
    a2a: evidence('e2e/remote_attempt_lifecycle_e2e.ts', 'Managed-cancel rule'),
    negative: evidence('e2e/management_deployment_schedule_e2e.mjs', 'auto-pauses'),
  },
  {
    id: 'dream-and-background-memory',
    native: evidence('e2e/managed_dream_real_eval_e2e.mjs', 'managed_dream_real_eval'),
    // Dream is a platform-owned Native auxiliary agent, but its frozen inputs
    // are backend-neutral Session facts. ACP coverage therefore belongs to the
    // real cross-runtime Memory lane, not a fictitious ACP Dream executor.
    ...acpPartitions('e2e/acp_runtime_memory_matrix_e2e.mjs', 'managed_memory_real_eval_acp_runtime_matrix'),
    // A2A selects a complete remote agent, not a local model. Therefore the
    // model grammar accepts executor-only A2A and local Dream mounts are
    // deliberately rejected rather than projected across the trust boundary.
    a2a: evidence('crates/control/awaken-config-service/src/managed_model_id.rs', 'A2A runtime'),
    negative: evidence('e2e/managed_dream_e2e.ts', 'completed Dream cannot be canceled'),
  },
  {
    id: 'provider-and-runtime-certification',
    native: evidence('e2e/provider_connection_matrix_real_e2e.mjs', 'AWAKEN_PROVIDER_COMPAT'),
    ...acpPartitions('e2e/acp_runtime_profiles.mjs', 'ACP_RUNTIME_IDS'),
    a2a: evidence('crates/control/awaken-config-service/src/managed_model_id.rs', 'A2A runtime'),
    negative: evidence('e2e/provider_compat_cases.test.mjs', 'fails before provider I/O'),
  },
];

const ids = groups.map(({ id }) => id);
assert.equal(new Set(ids).size, ids.length, 'feature group ids must be unique');
for (const group of groups) {
  for (const partition of PARTITIONS) {
    const item = group[partition];
    assert.ok(item, `${group.id} is missing ${partition} evidence`);
    const absolute = path.resolve(ROOT, item.file);
    assert.ok(fs.statSync(absolute, { throwIfNoEntry: false })?.isFile(), `${item.file} does not exist`);
    const source = fs.readFileSync(absolute, 'utf8');
    assert.ok(source.includes(item.token), `${group.id}/${partition}: ${item.file} lost token ${item.token}`);
  }
}

console.log(
  `NATIVE/ACP/A2A CAUSAL COVERAGE PASS: ${groups.length} feature groups × ${PARTITIONS.join('/')} partitions.`,
);
