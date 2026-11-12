"""Native fixture admission and independent domain-receipt expectations."""

from collections import Counter
from pathlib import Path
from _postgres_phase_plan import read_phases
from _native_postgres_gap_policy import WORK_CURRENT_CASES, WORK_PRODUCT_GAPS

# Minimum required domains are independent policy. The executable selectors
# still own exact argv; deriving those argv must not derive away an obligation.
POSTGRES_PACKAGES = {
    "core": (
        "awaken-reliability-testkit", "awaken-model-catalog-store", "awaken-credential-store",
        "awaken-data-subject-store", "awaken-captured-content-store", "awaken-work-store",
        "awaken-session-store", "awaken-memory-store", "awaken-dispatch", "awaken-skill-store", "awaken-work-store",
        "awaken-dispatch-http", "awaken-config-store", "awaken-session-store",
        "awaken-credential-store", "awaken-process-startup", "awaken-process-startup", "awaken-process-startup", "awaken-coordinator", "awaken-session-executor", "awaken-coordinator", "awaken-coordinator"),
    "storage": (
        "awaken-sql-storage", "awaken-session-store", "awaken-config-store", "awaken-store-postgres",
        "awaken-env-store", "awaken-file-store", "awaken-store-schema", "awaken-store-sqlite"),
    "storage-observations": ("awaken-session-store", "awaken-sql-storage", "awaken-store-postgres"),
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

CORE_SESSION_WORK_RELEASE_CASE_FLOORS = (
    'session_work_release::tests::live_postgres_cap_one_and_partial_owner_matrix',
    'session_work_release::tests::live_postgres_borrowed_primitives_preserve_the_callers_transaction',
    'session_work_release::tests::live_postgres_cap_two_serializes_duplicate_release_and_claim_takeover',
    'session_work_release::tests::live_postgres_rejects_stale_claim_and_changed_physical_evidence',
    'session_work_release::tests::live_postgres_work_release_rollback_and_epoch_owner_fences',
    'session_work_release::tests::live_postgres_shared_root_and_separate_work_database',
    'session_work_release::tests::http::live_postgres_http_cap_one_resumes_the_selected_session',
    'session_work_release::tests::http::live_postgres_http_cap_two_resumes_distinct_claims',
    'session_work_release::tests::http::live_postgres_http_cap_one_checkpoint_and_recovery_keep_exact_fences',
    'session_work_release::tests::http::live_postgres_http_resume_partial_affinity_keeps_original_owners',
    'session_work_release::tests::http::settlement::live_postgres_http_cap_one_settles_real_memory_once_without_held_guard',
    'session_work_release::tests::http::races::live_postgres_http_same_claim_cold_absence_recovers_the_one_accepted_receipt',
    'session_work_release::tests::http::races::live_postgres_http_cold_resume_takeover_refuses_before_claim_specific_root_write',
    'session_work_release::tests::http::settlement::live_postgres_http_memory_observer_release_preserves_a_successor_before_settlement_cas',
)

CORE_CLAIMED_COMMIT_CASE_FLOORS = (
    'runtime_authority::claimed_commit_tests::pool_tests::claimed_commit_shared_pool_cap_one_progresses',
    'runtime_authority::claimed_commit_tests::pool_tests::claimed_commit_shared_pool_cap_two_concurrent_progresses',
    'runtime_authority::claimed_commit_tests::pool_tests::claimed_postgres_cold_child_scope_replay_and_ordinary_binding',
    'runtime_authority::claimed_commit_tests::pool_tests::claimed_postgres_rollback_preserves_projection_and_receipt',
)
CORE_CLAIMED_CHILD_CASE = 'agent_runner::child_execution::tests::local_postgres_child_permission_recovers_with_its_own_prefix'
CORE_SELECTED_POSTGRES_WRITER_CASE_FLOORS = (
    'runtime_authority::claimed_commit_tests::selected_postgres_writer_rolls_back_late_failure_and_replays_on_one_connection',
    'runtime_authority::claimed_commit_tests::selected_postgres_writer_serializes_commit_and_takeover',
    'runtime_authority::claimed_commit_tests::selected_postgres_writer_fences_after_waiting_for_a_new_epoch',
    'runtime_authority::claimed_commit_tests::selected_postgres_writer_rolls_back_when_the_commit_connection_dies',
    'runtime_authority::claimed_commit_tests::selected_postgres_cold_reads_freeze_pages_and_resume_lifecycle_after_takeover',
)

# The race requires a live database; the integer conversion is a pure unit
# boundary retained here as an independently named outcome, not DB evidence.
STORAGE_SESSION_MUTATION_CASE_FLOORS = frozenset({
    'postgres::mutation::tests::postgres_borrowed_commit_keeps_caller_transaction_and_rolls_back_late_sql_failure',
    'postgres::mutation::tests::postgres_borrowed_commit_replays_exact_receipt_after_lost_acknowledgement',
    'postgres::mutation::tests::postgres_borrowed_commit_preserves_private_binding_noop_and_delete_receipts',
})

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
    if (not (STORAGE_SESSION_EXTRACTION_CASE_FLOORS | STORAGE_SESSION_MUTATION_CASE_FLOORS) <= floors
            or '--lib' not in before_runtime or 'postgres_' not in before_runtime
            or {'--test', '--tests', '--all-targets'} & set(before_runtime)
            or ('--features', 'test-support') not in pairs
            or {'--ignored', '--include-ignored', '--skip', '--exact'} & runtime_flags):
        raise ValueError('PostgreSQL selector changed independent Session extraction outcomes')



def _require_work_parity_phase(phases, root):
    work = [phase for phase in phases if phase.selector == 'backend_parity'
            and phase.package == 'awaken-work-store']
    if len(work) != 1:
        raise ValueError('PostgreSQL selector changed independent Work parity domain')
    arguments = work[0].arguments
    frontend = arguments[:arguments.index('--')] if '--' in arguments else arguments
    runtime = arguments[arguments.index('--') + 1:] if '--' in arguments else ()
    expected_frontend = (
        '-p', 'awaken-work-store', '--features', 'test-support',
        '--test', 'work_queue_conformance',
        *('--require-case=' + case for case in WORK_CURRENT_CASES),
    )
    expected_runtime = (
        *(value for case in WORK_PRODUCT_GAPS for value in ('--skip', case)),
        '--test-threads=1',
    )
    if frontend != expected_frontend or runtime != expected_runtime:
        raise ValueError('PostgreSQL selector changed independent Work parity/current-product-gap policy')
    source = (root / 'scripts/ci/pg_tests.sh').read_text(encoding='utf-8')
    definitions = source.split('checked_backend_parity() {\n')
    wrapper = definitions[1].split('\n}', 1)[0] + '\n' if len(definitions) == 2 else None
    expected_wrapper = (
        '  python3 scripts/ci/_native_postgres_gap_policy.py --check || return "$?"\n'
        '  python3 scripts/ci/_native_work_target_coverage.py --check || return "$?"\n'
        '  backend_parity\n'
    )
    direct = [line for line in source.splitlines() if line.strip() == 'backend_parity']
    checked = [line for line in source.splitlines()
               if line.strip() == 'checked_backend_parity || record_failure "$?"']
    if wrapper != expected_wrapper or len(direct) != 1 or len(checked) != 2:
        raise ValueError('PostgreSQL Work parity must validate its exact product-gap ledger before execution')



def _require_work_store_phase(phases, root):
    work = [phase for phase in phases if phase.selector == 'work_store_evidence'
            and phase.package == 'awaken-work-store']
    if len(work) != 1:
        raise ValueError('PostgreSQL selector changed independent Work store domain')
    expected_frontend = (
        '-p', 'awaken-work-store', '--features', 'test-support',
        '--lib', '--test', 'archive', '--test', 'environment_revision',
        '--test', 'lease_book_properties', '--test', 'work_identity',
        '--test', 'work_queue_parity', '--test', 'work_queue_properties',
        '--require-case=schema::identity_upgrade_tests::postgres_both_published_histories_preserve_live_leases_and_fence_deleted_ids',
        '--require-case=schema::ack_upgrade_tests::sqlite_both_published_histories_add_only_known_ack_generations',
        '--require-case=schema::ack_upgrade_tests::postgres_both_published_histories_add_only_known_ack_generations',
        '--require-case=schema::stop_upgrade_tests::sqlite_both_published_histories_preserve_unknown_old_stop_subjects',
        '--require-case=schema::stop_upgrade_tests::postgres_both_published_histories_preserve_unknown_old_stop_subjects',
        '--require-case=mutation::tests::stop_receipts_preserve_absence_and_reject_incomplete_or_ungranted_subjects',
        '--require-case=mutation::tests::stop_receipt_replay_requires_original_owner_and_current_generation',
        '--require-case=postgres_archive_keeps_existing_session_authority_after_reconnect',
        '--require-case=durable::postgres_allocator_work_and_watermark_roll_back_as_one_transaction',
        '--require-case=durable::postgres_independent_pools_allocate_concurrently_without_reset',
        '--require-case=durable::postgres_missing_malformed_and_exhausted_allocators_fail_closed',
        '--require-case=postgres_work_identity_is_not_reused_after_lifecycle_removal',
        '--require-case=restart::postgres_identity_high_watermark_survives_a_native_process_restart',
    )
    expected_runtime = (
        '--include-ignored', '--test-threads=1',
        '--skip', 'environment_revision_restart_child',
        '--skip', 'restart::identity_restart_child',
    )
    if work[0].arguments != (*expected_frontend, '--', *expected_runtime):
        raise ValueError('PostgreSQL selector changed independent Work store target/floor/helper policy')

    source = (root / 'scripts/ci/pg_tests.sh').read_text(encoding='utf-8')
    definitions = source.split('checked_work_store_evidence() {\n')
    wrapper = definitions[1].split('\n}', 1)[0] + '\n' if len(definitions) == 2 else None
    expected_wrapper = (
        '  python3 scripts/ci/_native_work_target_coverage.py --check || return "$?"\n'
        '  work_store_evidence\n'
    )
    direct = [line for line in source.splitlines() if line.strip() == 'work_store_evidence']
    checked = [line for line in source.splitlines()
               if line.strip() == 'checked_work_store_evidence || record_failure "$?"']
    if wrapper != expected_wrapper or len(direct) != 1 or len(checked) != 2:
        raise ValueError('PostgreSQL Work store must validate actual Cargo targets before execution')



def _require_session_work_release_phase(phases):
    prefix = 'session_work_release::tests::'
    selected = [phase for phase in phases
                if phase.selector == 'critical_durability_evidence'
                and phase.package == 'awaken-process-startup'
                and prefix in phase.arguments]
    expected = (
        '-p', 'awaken-process-startup', '--locked', '--lib', prefix,
        *('--require-case=' + case for case in CORE_SESSION_WORK_RELEASE_CASE_FLOORS),
        '--', '--ignored', '--test-threads=1',
    )
    if len(selected) != 1 or selected[0].arguments != expected or selected[0].environment:
        raise ValueError('PostgreSQL selector changed independent Session Work release outcomes')


def _require_claimed_commit_phases(phases):
    # Separate exact rows retain pool/child and production-selected writer
    # outcomes. Three Coordinator rows share a package, not a responsibility:
    # pool, selected writer and Application Access cannot replace one another.
    pool_prefix = 'runtime_authority::claimed_commit_tests::pool_tests::'
    writer_prefix = 'runtime_authority::claimed_commit_tests::selected_postgres_'
    application_access = 'application_access_store::tests::provisioned_postgres_application_access_durability_release_gate'
    expected = (
        ('awaken-coordinator', pool_prefix, (
            '-p', 'awaken-coordinator', '--locked', '--lib', pool_prefix,
            *('--require-case=' + case for case in CORE_CLAIMED_COMMIT_CASE_FLOORS),
            '--', '--ignored', '--test-threads=1',
        )),
        ('awaken-session-executor', None, (
            '-p', 'awaken-session-executor', '--locked', '--lib', CORE_CLAIMED_CHILD_CASE,
            '--require-case=' + CORE_CLAIMED_CHILD_CASE,
            '--', '--ignored', '--exact', '--test-threads=1',
        )),
        ('awaken-coordinator', writer_prefix, (
            '-p', 'awaken-coordinator', '--locked', '--lib', writer_prefix,
            *('--require-case=' + case for case in CORE_SELECTED_POSTGRES_WRITER_CASE_FLOORS),
            '--', '--test-threads=1',
        )),
        ('awaken-coordinator', application_access, (
            '-p', 'awaken-coordinator', '--locked', '--lib', application_access,
            '--require-case=' + application_access,
            '--', '--ignored', '--exact', '--test-threads=1',
        )),
    )
    for package, subject, arguments in expected:
        selected = [phase for phase in phases
                    if phase.selector == 'critical_durability_evidence' and phase.package == package
                    and (subject is None or subject in phase.arguments)]
        if len(selected) != 1 or selected[0].arguments != arguments or selected[0].environment:
            raise ValueError('PostgreSQL selector changed independent claimed-commit outcomes')


SESSION_CAPACITY_OBSERVATION_CASE = "postgres_session_capacity_reference_and_indexed_observation"
SESSION_CAPACITY_ROOTS = 100_000


def _require_session_capacity_observation_phase(phases):
    selected = [phase for phase in phases if phase.package == "awaken-session-store"]
    expected = (
        "--locked", "-p", "awaken-session-store", "--features", "test-support",
        "--test", "session_repo_conformance", SESSION_CAPACITY_OBSERVATION_CASE,
        "--require-case=" + SESSION_CAPACITY_OBSERVATION_CASE,
        "--", "--ignored", "--exact", "--test-threads=1",
    )
    if (len(selected) != 1 or selected[0].selector != "storage_observation_evidence"
            or selected[0].arguments != expected
            or selected[0].environment != (("AWAKEN_SESSION_PERF_ROOTS", "100000"),)):
        raise ValueError("PostgreSQL selector changed independent Session capacity observation/default workload")


def require_session_capacity_result(phase, results):
    """Qualify functional capacity observations; absent RSS never becomes zero evidence."""
    if phase.selector != "storage_observation_evidence" or phase.package != "awaken-session-store":
        return None
    import math
    import re
    lines = re.findall(r"^session_capacity (.+)$", results, re.M)
    if len(lines) != 1:
        raise ValueError("Session capacity requires one complete observation row")
    pairs = [token.split("=", 1) for token in lines[0].split()]
    expected = {"roots", "reference_ms", "indexed_ms", "startup_ms", "count_ms",
                "global_count", "high_water_kib", "owners"}
    if (any(len(pair) != 2 for pair in pairs) or len(pairs) != len(expected)
            or {pair[0] for pair in pairs} != expected):
        raise ValueError("Session capacity observation fields are missing or duplicated")
    fields = dict(pairs)
    try:
        counts = {key: int(fields[key]) for key in ("roots", "global_count", "owners", "high_water_kib")}
        times = {key: float(fields[key]) for key in ("reference_ms", "indexed_ms", "startup_ms", "count_ms")}
    except ValueError as error:
        raise ValueError("Session capacity observation is not numeric") from error
    if (counts["roots"] != SESSION_CAPACITY_ROOTS
            or counts["global_count"] != SESSION_CAPACITY_ROOTS + 1
            or counts["owners"] != 1 or counts["high_water_kib"] < 0
            or any(not math.isfinite(value) or value < 0 for value in times.values())):
        raise ValueError("Session capacity observation does not preserve the default workload and exact outcomes")
    peak = counts["high_water_kib"] or None
    return {
        "roots": counts["roots"], "global_count": counts["global_count"], "owners": counts["owners"],
        "timings_ms": times, "peak_resident_kib": peak,
        "unverified_metrics": [] if peak is not None else ["process peak RSS unavailable on this platform"],
        "scope": "Functional PostgreSQL capacity observation; no production capacity or baseline speedup claim.",
    }



def postgres_phases(scope, root=None):
    if scope not in POSTGRES_SELECTORS:
        raise ValueError("unsupported native PostgreSQL scope")
    phases = read_phases(root or Path(__file__).resolve().parents[2], POSTGRES_SELECTORS[scope])
    if Counter(phase.package for phase in phases) != Counter(POSTGRES_PACKAGES[scope]):
        raise ValueError("PostgreSQL selector changed independent required domain coverage")
    if scope == 'storage':
        _require_session_extraction_phase(phases)
    if scope == 'storage-observations':
        _require_session_capacity_observation_phase(phases)
    if scope == 'core':
        _require_work_parity_phase(phases, root or Path(__file__).resolve().parents[2])
        _require_work_store_phase(phases, root or Path(__file__).resolve().parents[2])
        _require_session_work_release_phase(phases)
        # Keep the original Dispatch observation's own refusal boundary;
        # the independently checked pool target is a different exact domain.
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
        _require_claimed_commit_phases(phases)
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
