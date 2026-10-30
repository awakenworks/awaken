"""Current Work parity and separately unaccepted, named product-gap reproducers.

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
)
WORK_GAP_BINDINGS = (
    ('B250', 'T09-C02', 'stale_same_owner_claimer_is_refused_after_reclaim'),
)
WORK_PRODUCT_GAPS = tuple('lease_acceptance::' + row[2] for row in WORK_GAP_BINDINGS)
WORK_SOURCE = 'crates/stores/awaken-work-store/tests/work_queue_conformance/lease_acceptance.rs'


def validate_work_product_gaps(data, root):
    from check_session_acceptance import _binding_errors

    bindings = {}
    for binding_id, _, _ in WORK_GAP_BINDINGS:
        matches = [item for item in data['bindings'] if item['id'] == binding_id]
        if len(matches) != 1:
            raise ValueError('Work product-gap binding missing or duplicated: ' + binding_id)
        bindings[binding_id] = matches[0]
    cases = {}
    for case_id in {row[1] for row in WORK_GAP_BINDINGS}:
        matches = [case for group in data['groups'] for case in group['cases'] if case['id'] == case_id]
        if len(matches) != 1:
            raise ValueError('Work product-gap case missing or duplicated: ' + case_id)
        case = cases[case_id] = matches[0]
        expected = {row[2] for row in WORK_GAP_BINDINGS if row[1] == case_id}
        gap = case.get('product_gap') or {}
        if (case.get('evidence_state') != 'product_gap'
                or not gap.get('reason') or not gap.get('follow_up')
                or set(gap.get('reproducers', [])) != expected):
            raise ValueError('Work product-gap classification changed: ' + case_id)
    lines = (root / WORK_SOURCE).read_text(encoding='utf-8').splitlines()
    result, cache = [], {WORK_SOURCE: lines}
    for binding_id, case_id, name in WORK_GAP_BINDINGS:
        binding, case = bindings[binding_id], cases[case_id]
        locations = binding.get('source_locations', [])
        if (binding.get('name') != name or binding.get('relation') != 'reproduces_product_gap'
                or binding.get('execution_witnesses') != []
                or set(binding.get('covers', {})) != {case_id}
                or binding['covers'][case_id] != {'oracles': [], 'negative_control': False}
                or binding_id not in case['binding_ids']
                or len(locations) != 1 or locations[0].get('path') != WORK_SOURCE):
            raise ValueError('Work product-gap identity changed: ' + binding_id)
        line = locations[0].get('line')
        if (not isinstance(line, int) or isinstance(line, bool) or not 0 < line <= len(lines)
                or 'fn ' + name + '(' not in lines[line - 1]
                or _binding_errors(binding, {}, root, cache)):
            raise ValueError('Work product-gap reproducer must remain exactly bound and ignored: ' + binding_id)
        result.append({'package': 'awaken-work-store', 'test': 'work_queue_conformance',
                       'case': 'lease_acceptance::' + name, 'binding': binding_id,
                       'acceptance_case': case_id, 'status': 'unverified-product-gap',
                       'reason': case['product_gap']['reason'],
                       'follow_up': case['product_gap']['follow_up']})
    return result


def postgres_product_gap_report(scope, root):
    if scope not in ('core', 'storage', 'resources', 'storage-observations'):
        raise ValueError('unsupported native PostgreSQL scope')
    if scope != 'core':
        return []
    from check_session_acceptance import _load
    return validate_work_product_gaps(_load(root), root)


def main(arguments):
    if arguments != ['--check']:
        raise ValueError('expected --check')
    report = postgres_product_gap_report('core', Path(__file__).resolve().parents[2])
    print(json.dumps({'scope': 'current backend parity', 'unverified_product_gaps': report,
                      'full_acceptance': False}, ensure_ascii=False))
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv[1:]))
