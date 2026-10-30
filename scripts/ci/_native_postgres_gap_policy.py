"""Current Work parity and explicitly unaccepted legacy identity limits.

Exclusions never become passing evidence. A changed classification, source
binding or ignore attribute requires a reviewed selection change. Unknown
ignored tests are still rejected by check_rust_test_execution.
"""
from pathlib import Path
import json
import sys

WORK_CURRENT_CASES = (
    'in_memory_backend_conforms',
    'sqlite_backend_conforms',
    'postgres_backend_conforms',
    'fixture_absence_and_configured_failure_are_distinct',
    'lease_acceptance::concurrent_takeover_admits_only_the_current_owner_on_volatile_and_file_sqlite',
    'lease_acceptance::postgres_work_lease_acceptance',
    'lease_acceptance::postgres_work_lease_acceptance_fixture_admission',
    'lease_acceptance::session_and_healthcheck_leases_are_taken_over_independently',
    'lease_acceptance::terminal_ack_retries_only_the_original_identity',
    'work_fixture_cleanup_preserves_another_live_schema',
    'lease_acceptance::ack_after_the_lease_lapsed_is_refused',
    'lease_acceptance::retried_ack_replays_the_original_acknowledgement',
    'lease_acceptance::acknowledgement_receipts_do_not_cross_lease_generations',
    'lease_acceptance::concurrent_acknowledgements_share_one_original_receipt',
    'lease_acceptance::sqlite_ack_checks_expiry_after_waiting_for_its_write_transaction',
    'lease_acceptance::postgres_ack_reply_belongs_to_its_committed_lease_snapshot',
    'lease_acceptance::retried_stop_after_lost_response_replays',
    'lease_acceptance::stop_replay::concurrent_stops_share_one_original_receipt',
    'lease_acceptance::stop_replay::old_stop_receipts_never_authorize_a_woken_generation',
    'lease_acceptance::stop_replay::sqlite_stop_receipt_survives_reopening_without_restoring_authority',
    'lease_acceptance::stop_replay::postgres_stop_receipt_survives_a_new_pool_without_restoring_authority',
    'lease_acceptance::stop_replay::postgres_stop_reply_belongs_to_its_committed_lease_snapshot',
    'lease_acceptance::credential_generation::same_credential_session_reclaim_fences_the_exact_generation',
    'lease_acceptance::credential_generation::owner_only_reclaim_preserves_the_credential_principal_boundary',
)
# No active ignored Work reproducer remains after the reviewed T09 calibration.
# The absent legacy process identity remains unaccepted, independently of skips.
WORK_GAP_BINDINGS = ()
WORK_PRODUCT_GAPS = ()
WORK_SOURCE = 'crates/stores/awaken-work-store/tests/work_queue_conformance/lease_acceptance/credential_generation.rs'
WORK_CALIBRATION_BINDINGS = (
    ('B250', 'owner_only_reclaim_preserves_the_credential_principal_boundary', [0, 2]),
    ('B262', 'same_credential_session_reclaim_fences_the_exact_generation', [0, 1, 2]),
)
RETIRED_PROCESS_ORACLE = 'stale_same_owner_claimer_is_refused_after_reclaim'


def validate_work_calibration(data, root):
    import hashlib
    import re

    cases = [case for group in data['groups'] for case in group['cases'] if case['id'] == 'T09-C02']
    if len(cases) != 1:
        raise ValueError('Work calibration case missing or duplicated')
    case = cases[0]
    unaccepted = (case.get('slices') or {}).get('unaccepted', [])
    if (case.get('evidence_state') != 'current_subset' or case.get('acceptance_state') != 'pending'
            or case.get('acceptance_scope') is not None
            or case.get('full_group_acceptance_implied') is not False
            or 'product_gap' in case
            or not any('Legacy owner-only' in text and 'same-credential process fencing' in text for text in unaccepted)
            or 'E17' not in case.get('calibration_note', '')
            or 'does not claim' not in case.get('calibration_note', '')):
        raise ValueError('Work calibration must retain the unaccepted legacy process boundary')
    body = (root / WORK_SOURCE).read_text(encoding='utf-8').encode('utf-8')
    lines = body.decode('utf-8').splitlines()
    for binding_id, name, oracles in WORK_CALIBRATION_BINDINGS:
        matches = [binding for binding in data['bindings'] if binding['id'] == binding_id]
        if len(matches) != 1:
            raise ValueError('Work calibration binding missing or duplicated: ' + binding_id)
        binding = matches[0]
        locations = binding.get('source_locations', [])
        if (binding.get('name') != name or binding.get('relation') != 'supports_subset_only'
                or binding.get('execution_witnesses') != []
                or binding.get('covers') != {'T09-C02': {'oracles': oracles, 'negative_control': False}}
                or binding_id not in case['binding_ids'] or not binding.get('review') or not binding.get('limitation')
                or len(locations) != 1 or locations[0].get('path') != WORK_SOURCE):
            raise ValueError('Work calibration identity changed: ' + binding_id)
        location = locations[0]
        line = location.get('line')
        if (not isinstance(line, int) or isinstance(line, bool) or not 0 < line <= len(lines)
                or 'async fn ' + name + '(' not in lines[line - 1]
                or not any(re.search(r'#\[tokio::test(?:\(|\])', text) for text in lines[max(0, line - 4):line])
                or any(re.search(r'#\[\s*ignore\b', text) for text in lines[max(0, line - 4):line])
                or location.get('file_sha256') != hashlib.sha256(body).hexdigest()):
            raise ValueError('Work calibration must bind the current ordinary test: ' + binding_id)
    retired = json.loads((root / 'scripts/ci/test_inventory_ledger.json').read_text(encoding='utf-8'))['removed']
    reason = retired.get(RETIRED_PROCESS_ORACLE, '')
    if (not all(token in reason for token in ('E17', 'unaccepted', 'credential',
                 *(name for _, name, _ in WORK_CALIBRATION_BINDINGS)))
            or re.search(r'\bfn\s+' + RETIRED_PROCESS_ORACLE + r'\s*\(',
                (root / 'crates/stores/awaken-work-store/tests/work_queue_conformance/lease_acceptance.rs').read_text(encoding='utf-8'))):
        raise ValueError('Work retired process oracle must preserve its historical limitation')
    return [{'acceptance_case': 'T09-C02', 'bindings': ['B250', 'B262'],
             'status': 'unaccepted-legacy-process-fencing', 'historical_evidence': 'E17',
             'reason': next(text for text in unaccepted if 'Legacy owner-only' in text),
             'full_acceptance': False}]


def validate_work_product_gaps(data, root):
    validate_work_calibration(data, root)
    return []


def postgres_work_limitations(scope, root):
    if scope not in ('core', 'storage', 'resources', 'storage-observations'):
        raise ValueError('unsupported native PostgreSQL scope')
    if scope != 'core':
        return []
    from check_session_acceptance import _load
    return validate_work_calibration(_load(root), root)


def postgres_product_gap_report(scope, root):
    postgres_work_limitations(scope, root)
    return []


def main(arguments):
    if arguments != ['--check']:
        raise ValueError('expected --check')
    root = Path(__file__).resolve().parents[2]
    print(json.dumps({'scope': 'current backend parity', 'unverified_product_gaps': [],
                      'unaccepted_limitations': postgres_work_limitations('core', root),
                      'full_acceptance': False}, ensure_ascii=False))
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv[1:]))
