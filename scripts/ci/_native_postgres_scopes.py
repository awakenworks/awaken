"""Native fixture admission and independent domain-receipt expectations."""

from collections import Counter
from pathlib import Path
from _postgres_phase_plan import read_phases

# Minimum required domains are independent policy. The executable selectors
# still own exact argv; deriving those argv must not derive away an obligation.
POSTGRES_PACKAGES = {
    "core": (
        "awaken-reliability-testkit", "awaken-model-catalog-store", "awaken-credential-store",
        "awaken-data-subject-store", "awaken-captured-content-store", "awaken-work-store",
        "awaken-session-store", "awaken-memory-store", "awaken-dispatch", "awaken-skill-store", "awaken-work-store",
        "awaken-dispatch-http", "awaken-config-store", "awaken-session-store",
        "awaken-credential-store", "awaken-process-startup", "awaken-process-startup", "awaken-coordinator"),
    "storage": (
        "awaken-sql-storage", "awaken-session-store", "awaken-config-store", "awaken-store-postgres",
        "awaken-env-store", "awaken-file-store", "awaken-store-schema", "awaken-store-sqlite"),
    "storage-observations": ("awaken-sql-storage", "awaken-store-postgres"),
    "resources": ('awaken-admin-config-api', 'awaken-resource-store', 'awaken-resource-persistence', 'awaken-session-application', 'awaken-protocol-managed', 'awaken-sandbox-policy-store', 'awaken-executable-agent-catalog', 'awaken-executable-environment-catalog', 'awaken-environment-application', 'awaken-environment-image-build'),
}


RESOURCE_APPLICATION_CASE_FLOORS = frozenset({
    'tests::authority::archive_race::postgres_archive_races_admission_replacement_and_recovery_through_root_cas',
    'tests::authority::archive_race::postgres_archive_races_admission_replacement_and_recovery_through_root_cas_fixture_admission',
    'tests::construction::required_tool_compiler_postgres_preserves_the_frozen_configuration',
    'tests::construction::required_tool_compiler_postgres_preserves_the_frozen_configuration_fixture_admission',
    'tests::creation::prepared_participants::postgres_prepared_participant_survives_root_failure_and_rebuild',
    'tests::creation::prepared_participants::postgres_prepared_participant_survives_root_failure_and_rebuild_fixture_admission',
    'tests::creation::time_assurance::postgres_mutation_time_receipts_reject_conflicts_and_recover_lost_ack',
    'tests::creation::time_assurance::postgres_mutation_time_receipts_reject_conflicts_and_recover_lost_ack_fixture_admission',
    'tests::event_batches::concurrent_consumption::postgres_independent_clients_consume_one_tool_ticket_identically_to_sqlite',
    'tests::event_batches::concurrent_consumption::postgres_independent_clients_consume_one_tool_ticket_identically_to_sqlite_fixture_admission',
    'tests::event_batches::postgres_competing_tool_replies_recheck_occupancy_after_root_cas_conflict',
    'tests::event_batches::postgres_competing_tool_replies_recheck_occupancy_after_root_cas_conflict_fixture_admission',
})


CORE_DISPATCH_OBSERVATION_CASE = 'postgres_exact_physical_execution_observations_use_the_original_store_clock'

# The race requires a live database; the integer conversion is a pure unit
# boundary retained here as an independently named outcome, not DB evidence.
STORAGE_SESSION_EXTRACTION_CASE_FLOORS = frozenset({
    'extraction::tests::postgres_extraction_execution_selection_and_first_claim_share_one_revision_fence',
    'extraction::tests::postgres_integer_conversion_rejects_clamping_and_lease_erasure',
})


POSTGRES_SELECTORS = {
    "core": ("backend_parity", "work_store_evidence", "http_transport_evidence", "critical_durability_evidence"),
    "storage": ("storage_evidence",),
    "storage-observations": ("storage_observation_evidence",),
    "resources": ("resources_evidence",),
}


def _require_session_extraction_phase(phases):
    store = [phase for phase in phases if phase.selector == 'storage_evidence'
             and phase.package == 'awaken-session-store']
    if len(store) != 1:
        raise ValueError('PostgreSQL selector changed independent Session extraction phase')
    arguments = store[0].arguments
    pairs = tuple(zip(arguments, arguments[1:]))
    before_runtime = arguments[:arguments.index('--')] if '--' in arguments else arguments
    runtime = arguments[arguments.index('--') + 1:] if '--' in arguments else ()
    runtime_flags = {argument.partition('=')[0] for argument in runtime if argument.startswith('--')}
    floors = {value.removeprefix('--require-case=') for value in before_runtime
              if value.startswith('--require-case=')}
    if (not STORAGE_SESSION_EXTRACTION_CASE_FLOORS <= floors
            or '--lib' not in before_runtime or 'postgres_' not in before_runtime
            or {'--test', '--tests', '--all-targets'} & set(before_runtime)
            or ('--features', 'test-support') not in pairs
            or {'--ignored', '--include-ignored', '--skip', '--exact'} & runtime_flags):
        raise ValueError('PostgreSQL selector changed independent Session extraction outcomes')


def postgres_phases(scope, root=None):
    if scope not in POSTGRES_SELECTORS:
        raise ValueError("unsupported native PostgreSQL scope")
    phases = read_phases(root or Path(__file__).resolve().parents[2], POSTGRES_SELECTORS[scope])
    if Counter(phase.package for phase in phases) != Counter(POSTGRES_PACKAGES[scope]):
        raise ValueError("PostgreSQL selector changed independent required domain coverage")
    if scope == 'storage':
        _require_session_extraction_phase(phases)
    if scope == 'core':
        dispatch = [phase for phase in phases if phase.package == 'awaken-dispatch']
        if len(dispatch) != 1:
            raise ValueError('PostgreSQL selector changed independent Dispatch observation phase')
        arguments = dispatch[0].arguments
        pairs = tuple(zip(arguments, arguments[1:]))
        runtime = arguments[arguments.index('--') + 1:] if '--' in arguments else ()
        runtime_flags = {argument.partition('=')[0] for argument in runtime if argument.startswith('--')}
        if (dispatch[0].selector != 'backend_parity'
                or ('--test', 'dispatch_queries') not in pairs
                or ('--features', 'test-support') not in pairs
                or '--require-case=' + CORE_DISPATCH_OBSERVATION_CASE not in arguments
                or CORE_DISPATCH_OBSERVATION_CASE not in arguments
                or '--exact' not in runtime_flags
                or {'--ignored', '--include-ignored', '--skip'} & runtime_flags):
            raise ValueError('PostgreSQL selector changed independent Dispatch observation outcome')
    for phase in phases:
        if (phase.selector == "critical_durability_evidence"
                and phase.package in ("awaken-config-store", "awaken-session-store", "awaken-credential-store")
                and phase.environment != (("AWAKEN_CRASH_BACKEND", "postgres"),)):
            raise ValueError("PostgreSQL selector changed independent crash backend policy")
    if scope == 'resources':
        floors = {
            value.removeprefix('--require-case=')
            for phase in phases
            if phase.package == 'awaken-session-application'
            for value in phase.arguments
            if value.startswith('--require-case=')
        }
        if not RESOURCE_APPLICATION_CASE_FLOORS <= floors:
            raise ValueError('PostgreSQL selector changed independent Session application outcomes')
    return phases



def postgres_plan(scope):
    if scope == "core":
        return [("postgres-" + name, name + ".log", ["scripts/ci/pg_tests.sh", "--" + name])
                for name in ("backend-parity", "work-store", "http", "critical")]
    if scope == "storage":
        return [("postgres-storage", "storage.log", ["scripts/ci/pg_tests.sh", "--storage"])]
    if scope == "storage-observations":
        return [("postgres-storage-observations", "storage-observations.log", ["scripts/ci/pg_tests.sh", "--storage-observations"])]
    if scope == "resources":
        return [("postgres-resources", "resources.log", ["scripts/ci/pg_tests.sh", "--resources"])]
    raise ValueError("unsupported native PostgreSQL scope")
