// Executable cause/effect coverage gate for the runtime-seam stages.
//
// Coverage unit follows docs/testing/e2e-cause-effect-graph-test-design.md: one
// externally observable functional obligation (F), not a Rust source line. An
// obligation counts only after its real-process TS/JS scenario exits successfully.
// Internal testkit structure, compile checks, and formal harness lines are reported
// by the release gate and are deliberately not mislabelled as E2E functionality.

import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

type Scenario = {
  id: string;
  file: string;
  postgres?: boolean;
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
  { id: 'pg_guard', file: 'e2e/postgres_claimed_commit_guard_e2e.ts', postgres: true },
  { id: 'pg_history', file: 'e2e/durable_pg_commit_e2e.mjs', postgres: true },
  { id: 'pg_wake', file: 'e2e/durable_pg_wake_e2e.mjs', postgres: true },
  { id: 'credential_reference_worker', file: 'e2e/credential_reference_worker_e2e.ts' },
  { id: 'credential_materialization_worker', file: 'e2e/credential_materialization_worker_e2e.ts' },
  { id: 'acp_credential_projection', file: 'e2e/acp_credential_projection_e2e.mjs' },
  { id: 'acp_projected_local', file: 'e2e/acp_projected_local_e2e.mjs' },
  { id: 'child_recovery', file: 'e2e/durable_child_sandbox_recovery_e2e.ts' },
  { id: 'durable_cancel', file: 'e2e/durable_worker_cancel_e2e.mjs' },
  { id: 'dispatch_metrics', file: 'e2e/dispatch_metrics_export_e2e.mjs' },
  { id: 'sandbox', file: 'e2e/sandbox_provisioning_e2e.mjs' },
  { id: 'memoryd_copy', file: 'e2e/memoryd_copy_lifecycle_e2e.mjs' },
  { id: 'resource_legacy_upgrade', file: 'e2e/resource_legacy_upgrade_e2e.mjs' },
  { id: 'resource_reclamation', file: 'e2e/resource_reclamation_e2e.mjs' },
  { id: 'resource_activation_recovery', file: 'e2e/resource_activation_recovery_e2e.mjs' },
  { id: 'resource_catalog_corruption', file: 'e2e/resource_catalog_corruption_e2e.mjs' },
  { id: 'resource_reclamation_faults', file: 'e2e/resource_reclamation_faults_e2e.mjs' },
  { id: 'memory_extraction_stage_recovery', file: 'e2e/managed_memory_extraction_stage_recovery_e2e.mjs' },
  { id: 'resource_ephemeral', file: 'e2e/resource_ephemeral_e2e.mjs' },
  { id: 'resource_scope_boundary', file: 'e2e/resource_scope_boundary_e2e.mjs' },
  { id: 'mcp_stdio', file: 'e2e/mcp_server_core_e2e.ts' },
  { id: 'mcp_http', file: 'e2e/mcp_streamable_http_e2e.ts' },
  { id: 'resource_plane_postgres', file: 'e2e/resource_plane_postgres_e2e.ts', postgres: true },
];

const obligations: Obligation[] = [
  { id: 'D0-01', stage: '0 durable dispatch seam', behavior: 'database-less worker commits through the cell single writer', scenario: 'worker_transport' },
  { id: 'D0-02', stage: '0 durable dispatch seam', behavior: 'at-least-once commit redelivery has one effect', scenario: 'worker_transport' },
  { id: 'D0-03', stage: '0 durable dispatch seam', behavior: 'enqueue → claim → settle crosses the real HTTP/store boundary', scenario: 'worker_transport' },
  { id: 'D0-04', stage: '0 durable dispatch seam', behavior: 'a final claim epoch settles at most once', scenario: 'worker_transport' },

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

  { id: 'D4-01', stage: '4 credential injection', behavior: 'snapshot inference_access survives durable enqueue and claim unchanged', scenario: 'worker_transport' },
  { id: 'D4-02', stage: '4 credential injection', behavior: 'claimed dispatch contains no provider key', scenario: 'worker_transport' },
  { id: 'D4-03', stage: '4 credential injection', behavior: 'real awaken-worker forwards the pinned reference to the inference materializer', scenario: 'credential_reference_worker' },
  { id: 'D4-04', stage: '4 credential injection', behavior: 'materialized executor drives the model result', scenario: 'credential_reference_worker' },
  { id: 'D4-05', stage: '4 credential injection', behavior: 'worker runs with provider key variables removed', scenario: 'credential_reference_worker' },
  { id: 'D4-06', stage: '4 credential injection', behavior: 'reference-routed result commits and settles exactly once', scenario: 'credential_reference_worker' },
  { id: 'D4-07', stage: '4 credential injection', behavior: 'production worker opens only credential materialization stores and calls the pinned endpoint', scenario: 'credential_materialization_worker' },
  { id: 'D4-08', stage: '4 credential injection', behavior: 'ACP native credential files remain host-brokered and unsafe projection fails closed before traffic', scenario: 'acp_credential_projection' },
  { id: 'D4-09', stage: '4 credential injection', behavior: 'the production composition projects endpoint and credential use once into the per-thread ACP sandbox', scenario: 'acp_projected_local' },
  { id: 'D4-10', stage: '4 credential injection', behavior: 'config-file ACP adapters receive a per-run materialized config home through the same local projection', scenario: 'acp_projected_local' },

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
  { id: 'D6-14', stage: '6 sandbox/recovery/metrics', behavior: 'fenced-commit counter exports from a stale in-process worker attempt' },

  { id: 'D7-01', stage: '7 resource persistence', behavior: 'File, Memory, Skill, and lifecycle adapters select one shared backend family', scenario: 'resource_plane_postgres' },
  { id: 'D7-02', stage: '7 resource persistence', behavior: 'resource data survives process and local-directory replacement', scenario: 'resource_plane_postgres' },
  { id: 'D7-03', stage: '7 resource persistence', behavior: 'cross-Workspace access fails closed without IAM data in resource storage', scenario: 'resource_plane_postgres' },
  { id: 'D7-04', stage: '7 resource persistence', behavior: 'copy realization creates Memory heads through the real memoryd process', scenario: 'memoryd_copy' },
  { id: 'D7-05', stage: '7 resource persistence', behavior: 'copy realization reconciles update/delete/create through CAS-aware harvest', scenario: 'memoryd_copy' },
  { id: 'D7-06', stage: '7 resource persistence', behavior: 'copy realization survives process replacement over one durable SQLite store', scenario: 'memoryd_copy' },
  { id: 'D7-07', stage: '7 resource persistence', behavior: 'non-UTF-8 files never become mutable Memory content', scenario: 'memoryd_copy' },
  { id: 'D7-08', stage: '7 resource persistence', behavior: 'legacy Memory history imports once and advances the canonical counter', scenario: 'resource_legacy_upgrade' },
  { id: 'D7-09', stage: '7 resource persistence', behavior: 'legacy Skill versions and support files become one canonical aggregate', scenario: 'resource_legacy_upgrade' },
  { id: 'D7-10', stage: '7 resource persistence', behavior: 'replacement process needs no legacy resource sidecar after upgrade', scenario: 'resource_legacy_upgrade' },
  { id: 'D7-10a', stage: '7 resource persistence', behavior: 'legacy owned Memory identities migrate once while unowned and duplicate rows remain quarantined', scenario: 'resource_legacy_upgrade' },
  { id: 'D7-11', stage: '7 resource persistence', behavior: 'authorized logical delete remains physically deferred by a live Session reference without IAM coupling', scenario: 'resource_reclamation' },
  { id: 'D7-12', stage: '7 resource persistence', behavior: 'SQLite Memory behavior config publishes with CAS and survives process replacement without pinning content', scenario: 'resource_legacy_upgrade' },
  { id: 'D7-13', stage: '7 resource persistence', behavior: 'Postgres Memory behavior config publishes with CAS and is shared across nodes', scenario: 'resource_plane_postgres' },
  { id: 'D7-13a', stage: '7 resource persistence', behavior: 'Postgres legacy Memory identities migrate without inferring ownership or replacing canonical history', scenario: 'resource_plane_postgres' },
  { id: 'D7-14', stage: '7 resource persistence', behavior: 'no-login/no-storage mode composes the volatile File, Memory, Skill, and lifecycle adapters under one explicit Workspace', scenario: 'resource_ephemeral' },
  { id: 'D7-15', stage: '7 resource persistence', behavior: 'one Memory API request atomically applies content plus rename-replace, while invalid paths and stale CAS leave head and history untouched', scenario: 'resource_ephemeral' },
  { id: 'D7-16', stage: '7 resource persistence', behavior: 'File, MemoryStore, and Skill adapters reject a missing trusted Workspace instead of inferring one from the Host', scenario: 'resource_scope_boundary' },
  { id: 'D7-17', stage: '7 resource persistence', behavior: 'persisted Prepared and Releasing Session resource generations converge after process death', scenario: 'resource_activation_recovery' },
  { id: 'D7-18', stage: '7 resource persistence', behavior: 'a missing current resource config fails closed and the frozen Session generation resumes after repair', scenario: 'resource_catalog_corruption' },
  { id: 'D7-19', stage: '7 resource persistence', behavior: 'guard, fence contention, late-reference, and release faults retry through the original purge intents', scenario: 'resource_reclamation_faults' },
  { id: 'D7-19a', stage: '7 resource persistence', behavior: 'Postgres reclamation fences recover from contention, late references, physical faults, and release faults', scenario: 'resource_plane_postgres' },
  { id: 'D7-20', stage: '7 resource persistence', behavior: 'Extracted and Stored Memory intents resume while stale mutations and unavailable extractors fail terminally', scenario: 'memory_extraction_stage_recovery' },

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
];

function docker(...args: string[]): string {
  return execFileSync('docker', args, { cwd: ROOT, encoding: 'utf8' }).trim();
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

async function main(): Promise<void> {
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

  const postgres = await startPostgres();
  // Keep stage ports below Linux's default ephemeral range so Docker's
  // dynamically-published Postgres port cannot occupy one. A per-process block
  // also lets independent stage runs coexist without sharing fixed ports.
  const portBase = 12_000 + (process.pid % 200) * 50;
  const passed = new Set<string>();
  try {
    for (const [index, scenario] of scenarios.entries()) {
      console.log(`\n[stage-e2e ${index + 1}/${scenarios.length}] ${scenario.id}`);
      const environment = {
        ...process.env,
        E2E_PORT: String(portBase + index),
        E2E_WORKER_PORT: String(portBase + 25 + index),
        ...(scenario.postgres
          ? {
              AWAKEN_DATABASE_URL: postgres.url,
              AWAKEN_E2E_POSTGRES_CONTAINER: postgres.container,
            }
          : {}),
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
