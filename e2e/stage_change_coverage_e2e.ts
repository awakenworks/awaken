// Executable cause/effect coverage gate for the runtime-seam stages.
//
// This executable test owns its coverage unit: one externally observable
// functional obligation, not a Rust source line. An obligation counts only after
// its real-process TS/JS scenario exits successfully.
// Internal testkit structure, compile checks, and formal harness lines are reported
// by the release gate and are deliberately not mislabelled as E2E functionality.
// Causes: C1=each unique obligation maps to one declared scenario; C2=that
// scenario passes, skips for an explicit infrastructure gap, or fails. Effects:
// E1=C1+pass marks only its mapped obligations covered; E2=skip records the gap;
// E3=duplicates, unknown mappings, failure, or uncovered obligations fail the gate.
// Constraints/invariant: executable scenario results in this file are the sole
// functional-coverage owner; source lines and separate design documents are not.
// Decision rules: G1=C1+pass=>E1; G2=C1+explicit-skip=>E2;
// G3=!C1|failure|uncovered=>E3.

import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

type Scenario = {
  id: string;
  file: string;
  postgres?: boolean;
  environment?: Record<string, string>;
};

type Obligation = {
  id: string;
  stage: string;
  behavior: string;
  scenario?: string;
};

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const MINIMUM = 0.95;
const scenarios: Scenario[] = [
  { id: 'worker_transport', file: 'e2e/worker_transport_e2e.mjs' },
  { id: 'remote_worker_recovery', file: 'e2e/remote_worker_recovery_e2e.ts' },
  { id: 'pg_guard', file: 'e2e/postgres_claimed_commit_guard_e2e.ts', postgres: true },
  { id: 'pg_history', file: 'e2e/durable_pg_commit_e2e.mjs', postgres: true },
  { id: 'pg_wake', file: 'e2e/durable_pg_wake_e2e.mjs', postgres: true },
  { id: 'credential_reference_worker', file: 'e2e/credential_reference_worker_e2e.ts' },
  { id: 'credential_materialization_worker', file: 'e2e/credential_materialization_worker_e2e.ts', postgres: true },
  { id: 'acp_projected_local', file: 'e2e/acp_projected_local_e2e.mjs' },
  { id: 'acp_projected_container', file: 'e2e/acp_projected_container_e2e.mjs' },
  { id: 'child_recovery', file: 'e2e/durable_child_sandbox_recovery_e2e.ts' },
  { id: 'durable_cancel', file: 'e2e/durable_worker_cancel_e2e.mjs' },
  { id: 'dispatch_metrics', file: 'e2e/dispatch_metrics_export_e2e.mjs' },
  { id: 'dispatch_fenced_metrics', file: 'e2e/dispatch_fenced_metrics_e2e.ts' },
  { id: 'sandbox', file: 'e2e/sandbox_provisioning_e2e.mjs' },
  { id: 'memory_mounter_copy', file: 'e2e/memory_mounter_copy_lifecycle_e2e.mjs' },
  { id: 'resource_reclamation', file: 'e2e/resource_reclamation_e2e.mjs' },
  { id: 'resource_activation_recovery', file: 'e2e/resource_activation_recovery_e2e.mjs' },
  { id: 'resource_catalog_corruption', file: 'e2e/resource_catalog_corruption_e2e.mjs' },
  { id: 'resource_reclamation_faults', file: 'e2e/resource_reclamation_faults_e2e.mjs' },
  { id: 'memory_extraction_stage_recovery', file: 'e2e/managed_memory_extraction_stage_recovery_e2e.mjs' },
  { id: 'managed_full_chain', file: 'e2e/managed_full_chain_e2e.mjs' },
  { id: 'resource_ephemeral', file: 'e2e/resource_ephemeral_e2e.mjs' },
  { id: 'resource_scope_boundary', file: 'e2e/resource_scope_boundary_e2e.mjs' },
  { id: 'container_provider_config', file: 'e2e/container_provider_configuration_e2e.ts' },
  {
    id: 'container_docker',
    file: 'e2e/managed_container_agent_e2e.mjs',
    environment: { AWAKEN_E2E_CONTAINER_ENGINE: 'docker', AWAKEN_E2E_REQUIRE_CONTAINER: '1' },
  },
  {
    id: 'container_podman',
    file: 'e2e/managed_container_agent_e2e.mjs',
    environment: { AWAKEN_E2E_CONTAINER_ENGINE: 'podman', AWAKEN_E2E_REQUIRE_CONTAINER: '1' },
  },
  { id: 'session_environment_faults', file: 'e2e/session_environment_recovery_fault_e2e.ts' },
  { id: 'file_workspace_ownership', file: 'e2e/file_workspace_ownership_e2e.ts' },
  { id: 'mcp_stdio', file: 'e2e/mcp_server_core_e2e.ts' },
  { id: 'mcp_http', file: 'e2e/mcp_streamable_http_e2e.ts' },
  { id: 'resource_plane_postgres', file: 'e2e/resource_plane_postgres_e2e.ts', postgres: true },
  { id: 'control_plane_postgres', file: 'e2e/control_plane_postgres_e2e.ts', postgres: true },
  { id: 'worker_resource_manifest', file: 'e2e/worker_resource_manifest_e2e.ts', postgres: true },
  { id: 'remote_attempt', file: 'e2e/remote_attempt_lifecycle_e2e.ts' },
  { id: 'remote_child_lifecycle', file: 'e2e/delegated_remote_lifecycle_e2e.ts' },
  { id: 'acp_permission', file: 'e2e/acp_permission_resume_e2e.mjs' },
  { id: 'acp_control', file: 'e2e/acp_control_lifecycle_e2e.ts' },
  { id: 'acp_ticket_corruption', file: 'e2e/acp_permission_corruption_e2e.ts' },
];

const obligations: Obligation[] = [
  { id: 'D0-01', stage: '0 durable dispatch seam', behavior: 'database-less worker commits through the cell single writer', scenario: 'worker_transport' },
  { id: 'D0-02', stage: '0 durable dispatch seam', behavior: 'at-least-once commit redelivery has one effect', scenario: 'worker_transport' },
  { id: 'D0-03', stage: '0 durable dispatch seam', behavior: 'enqueue → claim → settle crosses the real HTTP/store boundary', scenario: 'worker_transport' },
  { id: 'D0-04', stage: '0 durable dispatch seam', behavior: 'a final claim epoch settles at most once', scenario: 'worker_transport' },
  { id: 'D0-05', stage: '0 durable dispatch seam', behavior: 'a real Worker retries an already-applied commit after losing its receipt with the same logical operation', scenario: 'remote_worker_recovery' },
  { id: 'D0-06', stage: '0 durable dispatch seam', behavior: 'a replacement Worker reclaims a crashed Awaiting attempt from the committed recovery snapshot', scenario: 'remote_worker_recovery' },
  { id: 'D0-07', stage: '0 durable dispatch seam', behavior: 'the replacement resumes the exact remote context and commits one terminal effect', scenario: 'remote_worker_recovery' },
  { id: 'D0-08', stage: '0 durable dispatch seam', behavior: 'the superseded Worker epoch cannot replay its delayed claimed commit', scenario: 'remote_worker_recovery' },
  { id: 'D0-09', stage: '0 durable dispatch seam', behavior: 'a live authenticated remote claim publishes one idempotent scoped output File over HTTP', scenario: 'worker_transport' },
  { id: 'D0-10', stage: '0 durable dispatch seam', behavior: 'settlement fences every late remote artifact publication before Resource mutation', scenario: 'worker_transport' },

  { id: 'D1-01', stage: '1 backend conformance', behavior: 'SQLite durable authority serves queue semantics', scenario: 'worker_transport' },
  { id: 'D1-02', stage: '1 backend conformance', behavior: 'HTTP transport preserves the queue contract', scenario: 'worker_transport' },
  { id: 'D1-03', stage: '1 backend conformance', behavior: 'Postgres authority serves claim/recovery semantics', scenario: 'pg_guard' },
  { id: 'D1-04', stage: '1 backend conformance', behavior: 'Postgres commit history survives a process and local-dir replacement', scenario: 'pg_history' },
  { id: 'D1-05', stage: '1 backend conformance', behavior: 'pg_notify-wired pool drains before and after restart', scenario: 'pg_wake' },

  { id: 'D2-01', stage: '2 Postgres atomic fence', behavior: 'worker A obtains a concrete run claim', scenario: 'pg_guard' },
  { id: 'D2-02', stage: '2 Postgres atomic fence', behavior: 'FOR UPDATE commit guard excludes concurrent reclaim', scenario: 'pg_guard' },
  { id: 'D2-03', stage: '2 Postgres atomic fence', behavior: 'guard stays live across the actual delayed ThreadCommit', scenario: 'pg_guard' },
  { id: 'D2-04', stage: '2 Postgres atomic fence', behavior: 'reclaim after guard release advances the epoch', scenario: 'pg_guard' },
  { id: 'D2-05', stage: '2 Postgres atomic fence', behavior: 'superseded epoch cannot settle', scenario: 'pg_guard' },
  { id: 'D2-06', stage: '2 Postgres atomic fence', behavior: 'overlap produces exactly one transcript effect', scenario: 'pg_guard' },

  { id: 'D3-01', stage: '3 worker identity/scope', behavior: 'anonymous worker request is 401', scenario: 'worker_transport' },
  { id: 'D3-02', stage: '3 worker identity/scope', behavior: 'verified identity overrides forged owner/time input', scenario: 'worker_transport' },
  { id: 'D3-03', stage: '3 worker identity/scope', behavior: 'authenticated non-owner cannot commit a claim', scenario: 'worker_transport' },
  { id: 'D3-04', stage: '3 worker identity/scope', behavior: 'opaque verified execution scope survives dispatch', scenario: 'worker_transport' },
  { id: 'D3-05', stage: '3 worker identity/scope', behavior: 'real worker reuses one identity for claim/commit/settle', scenario: 'credential_reference_worker' },
  { id: 'D3-06', stage: '3 worker identity/scope', behavior: 'remote Session realization accepts renewal commands only', scenario: 'worker_transport' },
  { id: 'D3-07', stage: '3 worker identity/scope', behavior: 'Session realization owner and Runtime incarnation must match the authenticated Worker identity', scenario: 'worker_transport' },
  { id: 'D3-08', stage: '3 worker identity/scope', behavior: 'Session realization expiry cannot exceed the live Worker registry lease', scenario: 'worker_transport' },
  { id: 'D3-09', stage: '3 worker identity/scope', behavior: 'a preparing Session rejects Worker realization before credential or resource effects', scenario: 'worker_transport' },

  { id: 'D4-01', stage: '4 credential injection', behavior: 'complete published model candidate survives durable enqueue and claim unchanged', scenario: 'worker_transport' },
  { id: 'D4-02', stage: '4 credential injection', behavior: 'claimed dispatch contains no provider key', scenario: 'worker_transport' },
  { id: 'D4-03', stage: '4 credential injection', behavior: 'real awaken-worker forwards the pinned reference to the inference materializer', scenario: 'credential_reference_worker' },
  { id: 'D4-04', stage: '4 credential injection', behavior: 'materialized executor drives the model result', scenario: 'credential_reference_worker' },
  { id: 'D4-05', stage: '4 credential injection', behavior: 'worker runs with provider key variables removed', scenario: 'credential_reference_worker' },
  { id: 'D4-06', stage: '4 credential injection', behavior: 'reference-routed result commits and settles exactly once', scenario: 'credential_reference_worker' },
  { id: 'D4-07', stage: '4 credential injection', behavior: 'worker-local logout after placement fails exact use-time revalidation before Agent launch', scenario: 'credential_reference_worker' },
  { id: 'D4-08', stage: '4 credential injection', behavior: 'database-less production Worker consumes a typed recipient-bound projection and calls the pinned endpoint', scenario: 'credential_materialization_worker' },
  { id: 'D4-09', stage: '4 credential injection', behavior: 'the production composition projects endpoint and credential use once into the per-thread ACP sandbox', scenario: 'acp_projected_local' },
  { id: 'D4-10', stage: '4 credential injection', behavior: 'Codex rejects an incompatible bearer-only publication without restoring its removed environment credential path', scenario: 'acp_projected_local' },
  { id: 'D4-11', stage: '4 credential injection', behavior: 'the production container projection carries model access, MCP metadata, and a File input into one frozen run', scenario: 'acp_projected_container' },
  { id: 'D4-E01', stage: '4 credential injection', behavior: 'a recipient-bound envelope with a mismatched payload fingerprint fails before provider I/O', scenario: 'credential_materialization_worker' },
  { id: 'D4-E02', stage: '4 credential injection', behavior: 'an exact envelope cannot be replayed for a different endpoint binding', scenario: 'credential_materialization_worker' },
  { id: 'D4-E03', stage: '4 credential injection', behavior: 'an expired envelope fails closed before material resolution', scenario: 'credential_materialization_worker' },
  { id: 'D4-E04', stage: '4 credential injection', behavior: 'an envelope addressed to another Worker trust domain fails closed', scenario: 'credential_materialization_worker' },
  { id: 'D4-E05', stage: '4 credential injection', behavior: 'the exact recipient, target, use, payload and claim epoch resolve once and commit one provider reply', scenario: 'credential_materialization_worker' },


  { id: 'D5-01', stage: '5 durable child lifecycle', behavior: 'child has a first-class stable run identity', scenario: 'child_recovery' },
  { id: 'D5-02', stage: '5 durable child lifecycle', behavior: 'hard process crash occurs while child inference is in flight', scenario: 'child_recovery' },
  { id: 'D5-03', stage: '5 durable child lifecycle', behavior: 'replacement process reuses the same child run', scenario: 'child_recovery' },
  { id: 'D5-04', stage: '5 durable child lifecycle', behavior: 'recovered child result resumes and completes the parent', scenario: 'child_recovery' },
  { id: 'D5-05', stage: '5 durable child lifecycle', behavior: 'recovery does not duplicate child seed', scenario: 'child_recovery' },
  { id: 'D5-06', stage: '5 durable child lifecycle', behavior: 'recovery commits one child terminal result', scenario: 'child_recovery' },
  { id: 'D5-07', stage: '5 durable child lifecycle', behavior: 'awaiting durable run can be cancelled through HTTP', scenario: 'durable_cancel' },
  { id: 'D5-08', stage: '5 durable child lifecycle', behavior: 'cancelled run cannot recover or commit late', scenario: 'durable_cancel' },
  { id: 'D5-09', stage: '5 durable child lifecycle', behavior: 'unknown and duplicate cancel fail closed', scenario: 'durable_cancel' },

  { id: 'D6-01', stage: '6 sandbox/recovery/metrics', behavior: 'sandbox handle is durably bound before crash', scenario: 'child_recovery' },
  { id: 'D6-02', stage: '6 sandbox/recovery/metrics', behavior: 'native child shares parent session sandbox', scenario: 'child_recovery' },
  { id: 'D6-03', stage: '6 sandbox/recovery/metrics', behavior: 'replacement adopts the same sandbox handle', scenario: 'child_recovery' },
  { id: 'D6-04', stage: '6 sandbox/recovery/metrics', behavior: 'recovered parent and child dispatches settle and disappear', scenario: 'child_recovery' },
  { id: 'D6-05', stage: '6 sandbox/recovery/metrics', behavior: 'file/memory/repository resources realize in one sandbox', scenario: 'sandbox' },
  { id: 'D6-06', stage: '6 sandbox/recovery/metrics', behavior: 'dangling resource references fail closed', scenario: 'sandbox' },
  { id: 'D6-07', stage: '6 sandbox/recovery/metrics', behavior: 'claim metric exports over OTLP', scenario: 'dispatch_metrics' },
  { id: 'D6-08', stage: '6 sandbox/recovery/metrics', behavior: 'settle metric exports over OTLP', scenario: 'dispatch_metrics' },
  { id: 'D6-09', stage: '6 sandbox/recovery/metrics', behavior: 'drive-duration metric exports over OTLP', scenario: 'dispatch_metrics' },
  { id: 'D6-10', stage: '6 sandbox/recovery/metrics', behavior: 'queue-depth metric exports over OTLP', scenario: 'dispatch_metrics' },
  { id: 'D6-11', stage: '6 sandbox/recovery/metrics', behavior: 'commit count and duration export over OTLP', scenario: 'dispatch_metrics' },
  { id: 'D6-12', stage: '6 sandbox/recovery/metrics', behavior: 'in-flight metric exports over OTLP', scenario: 'dispatch_metrics' },
  { id: 'D6-13', stage: '6 sandbox/recovery/metrics', behavior: 'expired-lease recovery metric exports over OTLP', scenario: 'child_recovery' },
  { id: 'D6-14', stage: '6 sandbox/recovery/metrics', behavior: 'fenced-commit counter exports from a stale in-process worker attempt', scenario: 'dispatch_fenced_metrics' },
  { id: 'D6-15', stage: '6 sandbox/recovery/metrics', behavior: 'container tiers without matching build capabilities fail before accepting traffic', scenario: 'container_provider_config' },
  { id: 'D6-16', stage: '6 sandbox/recovery/metrics', behavior: 'a docker-only build still rejects unsupported podman and Kubernetes tiers', scenario: 'container_provider_config' },
  { id: 'D6-17', stage: '6 sandbox/recovery/metrics', behavior: 'one workspace revoke preserves a content-addressed blob owned by another workspace', scenario: 'file_workspace_ownership' },
  { id: 'D6-18', stage: '6 sandbox/recovery/metrics', behavior: 'last-owner deletion removes the blob and duplicate revoke fails closed', scenario: 'file_workspace_ownership' },
  { id: 'D6-19', stage: '6 sandbox/recovery/metrics', behavior: 'the managed API drives one Session-owned Docker environment across ACP, hand, resources, artifacts, and release', scenario: 'container_docker' },
  { id: 'D6-20', stage: '6 sandbox/recovery/metrics', behavior: 'the same managed lifecycle runs through the replaceable Podman provider without a parallel runtime path', scenario: 'container_podman' },
  { id: 'D6-21', stage: '6 sandbox/recovery/metrics', behavior: 'a Managed environment declaration selects Podman host-userland or an explicit image through the canonical SandboxSpec', scenario: 'container_podman' },
  { id: 'D6-22', stage: '6 sandbox/recovery/metrics', behavior: 'Managed network and resource-limit declarations reach the Podman run plan', scenario: 'container_podman' },
  { id: 'D6-23', stage: '6 sandbox/recovery/metrics', behavior: 'missing private-root directory and tarball declarations fail closed without falling back to the default image', scenario: 'container_podman' },
  { id: 'D6-24', stage: '6 sandbox/recovery/metrics', behavior: 'replacement rejects corrupt, cross-Session, wrong-provider, stopped and deleted durable environment bindings without creating a substitute', scenario: 'session_environment_faults' },

  { id: 'D7-01', stage: '7 resource persistence', behavior: 'File, Memory, Skill, and lifecycle adapters select one shared backend family', scenario: 'resource_plane_postgres' },
  { id: 'D7-02', stage: '7 resource persistence', behavior: 'resource data survives process and local-directory replacement', scenario: 'resource_plane_postgres' },
  { id: 'D7-03', stage: '7 resource persistence', behavior: 'cross-Workspace access fails closed without IAM data in resource storage', scenario: 'resource_plane_postgres' },
  { id: 'D7-04', stage: '7 resource persistence', behavior: 'copy realization creates Memory heads through the canonical MemoryMounter', scenario: 'memory_mounter_copy' },
  { id: 'D7-05', stage: '7 resource persistence', behavior: 'the canonical copy realization reconciles update/delete/create through CAS-aware harvest', scenario: 'memory_mounter_copy' },
  { id: 'D7-06', stage: '7 resource persistence', behavior: 'copy realization survives Repository and Mounter replacement over one durable SQLite store', scenario: 'memory_mounter_copy' },
  { id: 'D7-07', stage: '7 resource persistence', behavior: 'non-UTF-8 projected files never become mutable Memory content', scenario: 'memory_mounter_copy' },
  { id: 'D7-11', stage: '7 resource persistence', behavior: 'authorized logical delete remains physically deferred by a live Session reference without IAM coupling', scenario: 'resource_reclamation' },
  { id: 'D7-13', stage: '7 resource persistence', behavior: 'Postgres Memory behavior config publishes with CAS and is shared across nodes', scenario: 'resource_plane_postgres' },
  { id: 'D7-13a', stage: '7 resource persistence', behavior: 'Postgres retains one Resources Memory authority and exposes no retired Control Memory fallback', scenario: 'resource_plane_postgres' },
  { id: 'D7-14', stage: '7 resource persistence', behavior: 'local no-login mode composes isolated embedded File, Memory, Skill, and lifecycle adapters under one explicit Workspace', scenario: 'resource_ephemeral' },
  { id: 'D7-15', stage: '7 resource persistence', behavior: 'one Memory API request atomically applies content plus rename-replace, while invalid paths and stale CAS leave head and history untouched', scenario: 'resource_ephemeral' },
  { id: 'D7-16', stage: '7 resource persistence', behavior: 'File, MemoryStore, and Skill adapters reject a missing trusted Workspace instead of inferring one from the Host', scenario: 'resource_scope_boundary' },
  { id: 'D7-17', stage: '7 resource persistence', behavior: 'persisted Prepared and Releasing Session resource generations converge after process death', scenario: 'resource_activation_recovery' },
  { id: 'D7-18', stage: '7 resource persistence', behavior: 'a missing current resource config fails closed and the frozen Session generation resumes after repair', scenario: 'resource_catalog_corruption' },
  { id: 'D7-19', stage: '7 resource persistence', behavior: 'guard, fence contention, late-reference, and release faults retry through the original purge intents', scenario: 'resource_reclamation_faults' },
  { id: 'D7-19a', stage: '7 resource persistence', behavior: 'Postgres reclamation fences recover from contention, late references, physical faults, and release faults', scenario: 'resource_plane_postgres' },
  { id: 'D7-20', stage: '7 resource persistence', behavior: 'Extracted and Stored Memory intents resume while stale mutations and unavailable extractors fail terminally', scenario: 'memory_extraction_stage_recovery' },
  { id: 'D7-21', stage: '7 resource persistence', behavior: 'agent-authored Skill harvest is idempotent for equal bytes and appends one immutable changed version', scenario: 'managed_full_chain' },
  { id: 'D7-22', stage: '7 resource persistence', behavior: 'a cold remote worker realizes the frozen Session File manifest from shared resource truth', scenario: 'worker_resource_manifest' },
  { id: 'D7-23', stage: '7 resource persistence', behavior: 'an explicit empty manifest revokes a prior live projection and remains resource-capability constrained', scenario: 'worker_resource_manifest' },
  { id: 'D7-24', stage: '7 resource persistence', behavior: 'a mismatched dispatch and resource Workspace fails before sandbox creation', scenario: 'worker_resource_manifest' },
  { id: 'D7-25', stage: '7 resource persistence', behavior: 'a remote worker hash-verifies and materializes one frozen binary Skill bundle, then removes the exact tree on detach', scenario: 'worker_resource_manifest' },
  { id: 'D7-26', stage: '7 resource persistence', behavior: 'a remote worker uses the pinned Memory configuration to mount current mutable content from the shared data plane', scenario: 'worker_resource_manifest' },
  { id: 'D7-27', stage: '7 resource persistence', behavior: 'archiving a MemoryStore live denies a later claim even though its immutable configuration remains pinned', scenario: 'worker_resource_manifest' },
  { id: 'D7-28', stage: '7 resource persistence', behavior: 'all control-plane repository ports select Postgres without changing the HTTP publication or Session execution path', scenario: 'control_plane_postgres' },
  { id: 'D7-29', stage: '7 resource persistence', behavior: 'Postgres-backed catalog, credential, Agent publication, admin resources, webhooks, and Sessions survive process replacement', scenario: 'control_plane_postgres' },
  { id: 'D7-30', stage: '7 resource persistence', behavior: 'configuration through Session invocation and release produces a listed downloadable File with exact sandbox bytes', scenario: 'managed_full_chain' },
  { id: 'D7-A01', stage: '7 remote A2A attempt', behavior: 'managed config preserves and publishes the complete A2A backend binding', scenario: 'remote_attempt' },
  { id: 'D7-A02', stage: '7 remote A2A attempt', behavior: 'root remote attempt commits its opaque task reference before polling', scenario: 'remote_attempt' },
  { id: 'D7-A03', stage: '7 remote A2A attempt', behavior: 'replacement reattaches after hard crash without a second message send', scenario: 'remote_attempt' },
  { id: 'D7-A04', stage: '7 remote A2A attempt', behavior: 'every recovery poll addresses the pinned remote task identity', scenario: 'remote_attempt' },
  { id: 'D7-A05', stage: '7 remote A2A attempt', behavior: 'remote input-required resumes through the managed API', scenario: 'remote_attempt' },
  { id: 'D7-A06', stage: '7 remote A2A attempt', behavior: 'resume preserves the committed remote context and stable message identity', scenario: 'remote_attempt' },
  { id: 'D7-A07', stage: '7 remote A2A attempt', behavior: 'durable cancellation addresses the pinned remote task exactly once', scenario: 'remote_attempt' },
  { id: 'D7-A08', stage: '7 remote A2A attempt', behavior: 'cancelled remote attempt is no longer dispatchable', scenario: 'remote_attempt' },
  { id: 'D7-A08a', stage: '7 remote A2A attempt', behavior: 'active root polling cancellation aborts its pinned remote task', scenario: 'remote_attempt' },
  { id: 'D7-A08b', stage: '7 remote A2A attempt', behavior: 'remote poll and resume transport failures remain fail-closed errors', scenario: 'remote_attempt' },
  { id: 'D7-A08c', stage: '7 remote A2A attempt', behavior: 'cancellation does not re-cancel an already-terminal remote task', scenario: 'remote_attempt' },
  { id: 'D7-A09', stage: '7 remote A2A attempt', behavior: 'remote coordinated child input-required resumes through its exact pending ticket', scenario: 'remote_child_lifecycle' },
  { id: 'D7-A10', stage: '7 remote A2A attempt', behavior: 'remote child working state is polled to a terminal result', scenario: 'remote_child_lifecycle' },
  { id: 'D7-A11', stage: '7 remote A2A attempt', behavior: 'parent interrupt cancels the pinned remote child task exactly once', scenario: 'remote_child_lifecycle' },
  { id: 'D7-A12', stage: '7 remote A2A attempt', behavior: 'remote child 5xx fails closed without a fabricated result', scenario: 'remote_child_lifecycle' },

  { id: 'D7-ACP-01', stage: '7 governed ACP attempt', behavior: 'ACP permission asks use the Session policy and commit a durable resume ticket', scenario: 'acp_permission' },
  { id: 'D7-ACP-02', stage: '7 governed ACP attempt', behavior: 'Managed approval resumes only the exact pending ACP tool call', scenario: 'acp_permission' },
  { id: 'D7-ACP-03', stage: '7 governed ACP attempt', behavior: 'Managed denial selects the ACP agent reject option and terminates cleanly', scenario: 'acp_permission' },
  { id: 'D7-ACP-04', stage: '7 governed ACP attempt', behavior: 'live control pauses an ACP attempt at a safe boundary and commits a ManualPause ticket', scenario: 'acp_control' },
  { id: 'D7-ACP-05', stage: '7 governed ACP attempt', behavior: 'durable text resume validates and resumes exactly the paused ACP Run', scenario: 'acp_control' },
  { id: 'D7-ACP-06', stage: '7 governed ACP attempt', behavior: 'a continuation relaunch failure is committed instead of losing queued input', scenario: 'acp_control' },
  { id: 'D7-ACP-07', stage: '7 governed ACP attempt', behavior: 'restart rejects a malformed closed permission target without consuming or executing it', scenario: 'acp_ticket_corruption' },

  { id: 'D8-01', stage: '8 neutral MCP server core', behavior: 'newest and older protocol versions negotiate', scenario: 'mcp_stdio' },
  { id: 'D8-02', stage: '8 neutral MCP server core', behavior: 'unsupported version returns invalid params', scenario: 'mcp_stdio' },
  { id: 'D8-03', stage: '8 neutral MCP server core', behavior: 'notification produces no JSON-RPC response', scenario: 'mcp_stdio' },
  { id: 'D8-04', stage: '8 neutral MCP server core', behavior: 'ping and tools/list dispatch through facade', scenario: 'mcp_stdio' },
  { id: 'D8-05', stage: '8 neutral MCP server core', behavior: 'tools/call maps host result to MCP SDK shape', scenario: 'mcp_stdio' },
  { id: 'D8-06', stage: '8 neutral MCP server core', behavior: 'invalid params do not become a tool call', scenario: 'mcp_stdio' },
  { id: 'D8-07', stage: '8 neutral MCP server core', behavior: 'unknown method maps to -32601', scenario: 'mcp_stdio' },
  { id: 'D8-08', stage: '8 neutral MCP server core', behavior: 'progress is ordered and token-preserving', scenario: 'mcp_stdio' },
  { id: 'D8-09', stage: '8 neutral MCP server core', behavior: 'no progress occurs after final response', scenario: 'mcp_stdio' },
  { id: 'D8-10', stage: '8 neutral MCP server core', behavior: 'request-id cancellation stops the active tool call', scenario: 'mcp_stdio' },
  { id: 'D8-11', stage: '8 neutral MCP server core', behavior: 'HTTP bearer challenge and Accept preflight work', scenario: 'mcp_http' },
  { id: 'D8-12', stage: '8 neutral MCP server core', behavior: 'HTTP initialize opens a versioned session', scenario: 'mcp_http' },
  { id: 'D8-13', stage: '8 neutral MCP server core', behavior: 'HTTP notification returns 202 with empty body', scenario: 'mcp_http' },
  { id: 'D8-14', stage: '8 neutral MCP server core', behavior: 'HTTP version header gates non-initialize requests', scenario: 'mcp_http' },
  { id: 'D8-15', stage: '8 neutral MCP server core', behavior: 'progress and one final response use ordered SSE envelopes', scenario: 'mcp_http' },
  { id: 'D8-16', stage: '8 neutral MCP server core', behavior: 'tools/list_changed reaches a standing SSE client', scenario: 'mcp_http' },
  { id: 'D8-17', stage: '8 neutral MCP server core', behavior: 'DELETE closes session and stale reuse is 404', scenario: 'mcp_http' },

  // G42/G43 promotion decision table. These rows consolidate existing real
  // process effects; they do not create another Session/MCP or credential test
  // path. G43-P07 is deliberately a fail-closed cell: positive Worker-held
  // substitution remains unpromoted until a deployment provider proves both
  // substitution and no-bypass networking.
  //
  // | Rule | exact generation/source | live claim/holder | provider proof | Effect |
  // | G42-P01..P06 | yes | yes | n/a | freeze/realize/recover one generation |
  // | G43-P01..P06 | yes | yes | n/a | materialize exact binding or reject |
  // | G43-P07 | yes | yes | no | reject before launch; never downgrade |
  { id: 'G43-P01', stage: '9 guardrail promotion evidence', behavior: 'credential materialization binds exact source, recipient, target, usage, payload and claim epoch', scenario: 'credential_materialization_worker' },
  { id: 'G43-P02', stage: '9 guardrail promotion evidence', behavior: 'mismatched payload or target cannot replay an envelope', scenario: 'credential_materialization_worker' },
  { id: 'G43-P03', stage: '9 guardrail promotion evidence', behavior: 'expired or wrong-recipient material fails before provider I/O', scenario: 'credential_materialization_worker' },
  { id: 'G43-P04', stage: '9 guardrail promotion evidence', behavior: 'ambient provider variables cannot replace the published credential reference', scenario: 'credential_reference_worker' },
  { id: 'G43-P05', stage: '9 guardrail promotion evidence', behavior: 'revoked worker-local material fails exact use-time validation before launch', scenario: 'credential_reference_worker' },
  { id: 'G43-P06', stage: '9 guardrail promotion evidence', behavior: 'ACP receives the publication-pinned endpoint, model, credential revision and isolated config home', scenario: 'acp_projected_local' },
  { id: 'G43-P07', stage: '9 guardrail promotion evidence', behavior: 'authenticated container ACP MCP fails closed without provider substitution and no-bypass proof', scenario: 'acp_projected_container' },
];

function docker(...args: string[]): string {
  return execFileSync('docker', args, {
    cwd: ROOT,
    encoding: 'utf8',
    timeout: 30_000,
  }).trim();
}

function unavailableOptionalInfrastructure(scenario: Scenario): string | undefined {
  if (process.env.AWAKEN_E2E_ALLOW_MISSING_CONTAINER_ENGINES !== '1') return undefined;

  // Infrastructure cause graph: C1 scenario needs an external engine; C2 the
  // engine is installed; C3 the developer explicitly allows a local gap.
  // Only C1+!C2+C3 skips execution, and the scenario remains absent from `passed`
  // so its obligations are reported as uncovered. Default CI (!C3) stays strict.
  //
  // | Rule | engine needed | available | allow gap | Result |
  // |---|---|---|---|---|
  // | I1 | T | T | any | run and count only on success |
  // | I2 | T | F | F | run/fail strict |
  // | I3 | T | F | T | explicit uncovered gap; continue |
  const engine = scenario.id === 'container_docker'
    ? 'docker'
    : scenario.id === 'container_podman'
      ? 'podman'
      : undefined;
  if (engine) {
    const probe = spawnSync(engine, ['version'], {
      cwd: ROOT,
      stdio: 'ignore',
      timeout: 10_000,
    });
    if (probe.status !== 0) return `${engine} runtime is unavailable`;
  }
  return undefined;
}

async function startPostgres(): Promise<{ container: string; url: string }> {
  const container = `awaken-stage-e2e-pg-${process.pid}`;
  docker(
    'run',
    '-d',
    '--name',
    container,
    '-e',
    'POSTGRES_PASSWORD=test',
    '-e',
    'POSTGRES_DB=awaken',
    '-p',
    '127.0.0.1::5432',
    '--health-cmd=pg_isready -U postgres -d awaken',
    '--health-interval=1s',
    '--health-timeout=2s',
    '--health-retries=30',
    'postgres:16-alpine',
  );
  const deadline = Date.now() + 60_000;
  while (Date.now() <= deadline) {
    const health = docker('inspect', '--format', '{{.State.Health.Status}}', container);
    if (health === 'healthy') {
      const mapping = docker('port', container, '5432/tcp').split('\n')[0];
      const port = mapping.slice(mapping.lastIndexOf(':') + 1);
      return { container, url: `postgres://postgres:test@127.0.0.1:${port}/awaken` };
    }
    if (health === 'unhealthy') throw new Error('disposable Postgres became unhealthy');
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error('timed out waiting for disposable Postgres');
}

const STAGE_PORT_FIRST = 12_000;
const STAGE_PORT_COUNT = 15_000;

async function canBind(port: number): Promise<boolean> {
  return new Promise((resolve, reject) => {
    const reservation = net.createServer();
    reservation.once('error', (error: NodeJS.ErrnoException) => {
      if (error.code === 'EADDRINUSE') resolve(false);
      else reject(error);
    });
    reservation.listen(port, '127.0.0.1', () => {
      reservation.close((error) => {
        if (error) reject(error);
        else resolve(true);
      });
    });
  });
}

function stagePortAllocator(): () => Promise<number> {
  let cursor = (process.pid * 53) % STAGE_PORT_COUNT;
  return async () => {
    for (let attempt = 0; attempt < STAGE_PORT_COUNT; attempt += 1) {
      const port = STAGE_PORT_FIRST + cursor;
      cursor = (cursor + 1) % STAGE_PORT_COUNT;
      if (await canBind(port)) return port;
    }
    throw new Error('no free non-ephemeral stage E2E port');
  };
}

async function main(): Promise<void> {
  assert.equal(
    new Set(obligations.map((obligation) => obligation.id)).size,
    obligations.length,
    'functional obligation ids must be globally unique',
  );
  for (const scenario of scenarios) {
    assert.ok(
      obligations.some((obligation) => obligation.scenario === scenario.id),
      `scenario ${scenario.id} must cover at least one declared obligation`,
    );
  }
  for (const obligation of obligations.filter((entry) => entry.scenario)) {
    assert.ok(
      scenarios.some((scenario) => scenario.id === obligation.scenario),
      `${obligation.id} references an unknown scenario`,
    );
  }

  const fromIndex = process.argv.indexOf('--from');
  const fromId = fromIndex === -1 ? undefined : process.argv[fromIndex + 1];
  if (fromIndex !== -1 && !fromId) throw new Error('--from requires a scenario id');
  const firstScenario = fromId === undefined
    ? 0
    : scenarios.findIndex((scenario) => scenario.id === fromId);
  if (firstScenario === -1) throw new Error(`unknown --from scenario ${fromId}`);
  const selectedScenarios = scenarios.slice(firstScenario);

  const postgres = await startPostgres();
  // Keep stage ports below Linux's default ephemeral range, but probe each port
  // before handing it to a child. PID-derived fixed blocks can alias and also
  // collide with unrelated host services during concurrent CI/local runs.
  const nextStagePort = stagePortAllocator();
  const passed = new Set<string>();
  try {
    for (const [index, scenario] of selectedScenarios.entries()) {
      console.log(`\n[stage-e2e ${firstScenario + index + 1}/${scenarios.length}] ${scenario.id}`);
      const infrastructureGap = unavailableOptionalInfrastructure(scenario);
      if (infrastructureGap) {
        console.log(`  explicit infrastructure gap: ${infrastructureGap}`);
        continue;
      }
      const port = await nextStagePort();
      const workerPort = await nextStagePort();
      const configPort = await nextStagePort();
      const environment = {
        ...process.env,
        E2E_PORT: String(port),
        E2E_WORKER_PORT: String(workerPort),
        E2E_CONFIG_PORT: String(configPort),
        ...(scenario.postgres
          ? {
              SESSION_DEPLOYMENT_DATABASE_URL: postgres.url,
              AWAKEN_E2E_POSTGRES_CONTAINER: postgres.container,
            }
          : {}),
        ...scenario.environment,
      };
      const result = spawnSync(process.execPath, [scenario.file], {
        cwd: ROOT,
        env: environment,
        stdio: 'inherit',
      });
      assert.equal(result.status, 0, `${scenario.id} failed with status ${result.status}`);
      passed.add(scenario.id);
    }
  } finally {
    try {
      docker('rm', '-f', postgres.container);
    } catch (error) {
      console.error(`failed to remove disposable Postgres ${postgres.container}: ${error}`);
    }
  }

  if (fromId !== undefined) {
    console.log(
      `\nSTAGE CHANGE E2E PARTIAL PASS: ${selectedScenarios.length} scenario(s) from ${fromId}.`,
    );
    return;
  }

  const covered = obligations.filter((obligation) => obligation.scenario && passed.has(obligation.scenario));
  const uncovered = obligations.filter((obligation) => !covered.includes(obligation));
  const ratio = covered.length / obligations.length;
  const stages = [...new Set(obligations.map((obligation) => obligation.stage))];
  console.log('\nStage change E2E functional coverage:');
  for (const stage of stages) {
    const stageObligations = obligations.filter((obligation) => obligation.stage === stage);
    const stageCovered = stageObligations.filter((obligation) => covered.includes(obligation));
    console.log(`  ${stage}: ${stageCovered.length}/${stageObligations.length}`);
  }
  for (const gap of uncovered) {
    console.log(`  explicit gap ${gap.id}: ${gap.behavior}`);
  }
  console.log(
    `STAGE CHANGE E2E COVERAGE ${(ratio * 100).toFixed(2)}% ` +
      `(${covered.length}/${obligations.length}; required > ${(MINIMUM * 100).toFixed(0)}%)`,
  );
  assert.ok(ratio > MINIMUM, `functional E2E coverage ${(ratio * 100).toFixed(2)}% must exceed 95%`);
}

main().catch((error) => {
  console.error('STAGE CHANGE E2E COVERAGE FAIL:', error);
  process.exitCode = 1;
});
