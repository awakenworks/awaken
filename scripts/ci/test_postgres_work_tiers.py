"""Pure selector and ledger admission controls; never backend execution proof."""
import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from _native_postgres_gap_policy import (
    WORK_CURRENT_CASES, WORK_GAP_BINDINGS, WORK_PRODUCT_GAPS, WORK_SOURCE,
    postgres_product_gap_report, validate_work_product_gaps, validate_work_calibration, postgres_work_limitations,
)
from _native_postgres_scopes import postgres_phases

ROOT = Path(__file__).resolve().parents[2]
EXPECTED_GAPS = ()
CALIBRATED = (
    ('B250', 'owner_only_reclaim_preserves_the_credential_principal_boundary', [0, 2]),
    ('B262', 'same_credential_session_reclaim_fences_the_exact_generation', [0, 1, 2]),
)


class WorkParitySelectionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source = (ROOT / 'scripts/ci/pg_tests.sh').read_text(encoding='utf-8')

    def verify(self, source):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = root / 'scripts/ci/pg_tests.sh'
            script.parent.mkdir(parents=True)
            script.write_text(source, encoding='utf-8')
            return postgres_phases('core', root)

    def changed_work(self, transform):
        start = self.source.index('  pg_required_test -p awaken-work-store --features test-support --test work_queue_conformance ')
        end = self.source.index('\n  pg_required_test', start + 1)
        original = self.source[start:end]
        changed = transform(original)
        self.assertNotEqual(original, changed)
        return self.source[:start] + changed + self.source[end:]

    def test_all_current_cases_are_required_without_retired_gap_exclusions(self):
        phases = self.verify(self.source)
        self.assertEqual(len(phases), 18)
        work = [p for p in phases if p.selector == 'backend_parity' and p.package == 'awaken-work-store']
        self.assertEqual(len(work), 1)
        self.assertEqual(len(WORK_CURRENT_CASES), 24)
        self.assertEqual(WORK_GAP_BINDINGS, EXPECTED_GAPS)
        self.assertEqual(WORK_PRODUCT_GAPS, tuple('lease_acceptance::' + row[2] for row in EXPECTED_GAPS))
        floors = [a.removeprefix('--require-case=') for a in work[0].arguments if a.startswith('--require-case=')]
        self.assertEqual(tuple(floors), WORK_CURRENT_CASES)
        for _, name, _ in CALIBRATED:
            self.assertIn('lease_acceptance::credential_generation::' + name, floors)
        self.assertNotIn('--ignored', work[0].arguments)

    def test_each_current_case_is_mandatory_and_cannot_be_replaced(self):
        for case in WORK_CURRENT_CASES:
            for replacement in ('', '--require-case=unrelated'):
                changed = self.changed_work(lambda text: text.replace('--require-case=' + case, replacement, 1))
                with self.subTest(case=case, replacement=replacement), self.assertRaisesRegex(ValueError, 'independent Work'):
                    self.verify(changed)

    def test_resolved_ack_and_stop_observations_cannot_be_excluded_again(self):
        cases = (
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
        for case in cases:
            self.assertIn(case, WORK_CURRENT_CASES)
            self.assertNotIn(case, WORK_PRODUCT_GAPS)
            changed = self.changed_work(lambda text: text.replace('--test-threads=1',
                '--skip ' + case + ' --test-threads=1', 1))
            with self.subTest(case=case), self.assertRaisesRegex(ValueError, 'independent Work'):
                self.verify(changed)


    def test_no_current_case_can_be_skipped_and_retired_oracle_cannot_return_as_skip(self):
        for case in (*WORK_CURRENT_CASES, 'lease_acceptance::stale_same_owner_claimer_is_refused_after_reclaim'):
            changed = self.changed_work(lambda text: text.replace('--test-threads=1',
                '--skip ' + case + ' --test-threads=1', 1))
            with self.subTest(case=case), self.assertRaisesRegex(ValueError, 'independent Work'):
                self.verify(changed)

    def test_target_features_filter_and_ignored_flags_cannot_change_current_scope(self):
        mutations = (
            lambda text: text.replace('--test work_queue_conformance', '--test other_target', 1),
            lambda text: text.replace('--features test-support', '--features other', 1),
            lambda text: text.replace('    -- \\\n', '    other_filter -- \\\n', 1),
            lambda text: text.replace('--test-threads=1', '--ignored --test-threads=1', 1),
            lambda text: text.replace('--test-threads=1', '--include-ignored --test-threads=1', 1),
            lambda text: text.replace('--test-threads=1', '--skip=lease_acceptance --test-threads=1', 1),
        )
        for index, mutation in enumerate(mutations):
            with self.subTest(index=index), self.assertRaisesRegex(ValueError, 'independent Work'):
                self.verify(self.changed_work(mutation))

    def test_every_actual_backend_caller_validates_the_ledger_and_propagates_refusal(self):
        marker = 'python3 scripts/ci/_native_postgres_gap_policy.py --check || return "$?"'
        self.assertEqual(self.source.count(marker), 1)
        mutations = (
            self.source.replace(marker, 'true', 1),
            self.source.replace(marker, marker.replace('--check', '--skip'), 1),
            self.source.replace(marker, marker.replace('return "$?"', 'true'), 1),
            self.source.replace('checked_backend_parity || record_failure "$?"', 'backend_parity', 1),
            self.source.replace('checked_backend_parity || record_failure "$?"', 'checked_backend_parity', 1),
        )
        for index, source in enumerate(mutations):
            with self.subTest(index=index), self.assertRaisesRegex(ValueError, 'validate its exact product-gap ledger'):
                self.verify(source)

    def test_unknown_ignored_cases_still_fail_the_real_execution_reconciler(self):
        import nextest_execution as execution
        inventory = execution.listing('current', 'unknown_new_gap', ignored=('unknown_new_gap',))
        observed = execution.qualify(inventory, execution.junit('current'), 0, ['current'])
        self.assertIn('selected tests are #[ignore]d: unknown_new_gap', observed['errors'])


class WorkCalibrationAdmissionTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.path = self.root / WORK_SOURCE
        self.path.parent.mkdir(parents=True)
        lines, bindings = [], []
        for binding_id, name, oracles in CALIBRATED:
            lines += ['#[tokio::test(flavor = "multi_thread", worker_threads = 4)]',
                      'async fn ' + name + '() { assert!(true); }', '']
            bindings.append({'id': binding_id, 'name': name, 'relation': 'supports_subset_only',
                'execution_witnesses': [], 'review': 'Supported credential and generation boundary',
                'limitation': 'No physical incarnation claim',
                'source_locations': [{'path': WORK_SOURCE, 'line': len(lines) - 1}],
                'covers': {'T09-C02': {'oracles': oracles, 'negative_control': False}}})
        self.path.write_bytes(('\n'.join(lines) + '\n').encode())
        for binding in bindings:
            binding['source_locations'][0]['file_sha256'] = hashlib.sha256(self.path.read_bytes()).hexdigest()
        self.old_source = self.path.parent.parent / 'lease_acceptance.rs'
        self.old_source.write_bytes(b'mod credential_generation;\n')
        case = {'id': 'T09-C02', 'evidence_state': 'current_subset', 'acceptance_state': 'pending',
                'acceptance_scope': None, 'full_group_acceptance_implied': False,
                'binding_ids': ['B250', 'B262'],
                'slices': {'unaccepted': ['Legacy owner-only requests: same-credential process fencing remains absent']},
                'calibration_note': 'E17 keeps the original failure; this does not claim implementation.'}
        self.data = {'groups': [{'cases': [case]}], 'bindings': bindings}
        self.ledger = self.root / 'scripts/ci/test_inventory_ledger.json'
        self.ledger.parent.mkdir(parents=True)
        self.ledger.write_text(json.dumps({'removed': {
            'stale_same_owner_claimer_is_refused_after_reclaim':
                'E17 credential process distinction unaccepted; ' + ' '.join(row[1] for row in CALIBRATED)}}), encoding='utf-8')

    def verify(self, data=None):
        return validate_work_calibration(self.data if data is None else data, self.root)

    def test_no_skips_does_not_mean_legacy_process_fencing_was_accepted(self):
        report = self.verify()
        self.assertEqual(len(report), 1)
        self.assertEqual(report[0]['status'], 'unaccepted-legacy-process-fencing')
        self.assertEqual(report[0]['historical_evidence'], 'E17')
        self.assertFalse(report[0]['full_acceptance'])
        self.assertEqual(validate_work_product_gaps(self.data, self.root), [])
        for scope in ('resources', 'storage', 'storage-observations'):
            self.assertEqual(postgres_work_limitations(scope, self.root), [])
        with self.assertRaises(ValueError):
            postgres_product_gap_report('unknown', self.root)

    def test_actual_ledger_retains_exact_calibrated_limits(self):
        report = postgres_work_limitations('core', ROOT)
        self.assertEqual(report[0]['bindings'], ['B250', 'B262'])
        self.assertIn('same-credential process fencing', report[0]['reason'])

    def test_promotion_erased_limitation_or_restored_gap_requires_review(self):
        changes = {'evidence_state': 'current_clause_complete', 'acceptance_state': 'accepted',
                   'acceptance_scope': 'all', 'full_group_acceptance_implied': True,
                   'slices': {'unaccepted': []}, 'calibration_note': 'fixed',
                   'product_gap': {'reason': 'restored'}}
        for field, value in changes.items():
            data = copy.deepcopy(self.data)
            data['groups'][0]['cases'][0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.verify(data)

    def test_missing_duplicate_wrong_or_promoted_binding_is_refused(self):
        for index in range(2):
            for field, value in (('name', 'other'), ('relation', 'proves_clause'),
                                 ('execution_witnesses', [{'status': 'passed'}]),
                                 ('covers', {'T09-C02': {'oracles': [0], 'negative_control': True}})):
                data = copy.deepcopy(self.data)
                data['bindings'][index][field] = value
                with self.subTest(index=index, field=field), self.assertRaises(ValueError):
                    self.verify(data)
            for mutation in ('missing', 'duplicate', 'unbound'):
                data = copy.deepcopy(self.data)
                if mutation == 'missing':
                    data['bindings'].pop(index)
                elif mutation == 'duplicate':
                    data['bindings'].append(copy.deepcopy(data['bindings'][index]))
                else:
                    data['groups'][0]['cases'][0]['binding_ids'].remove(CALIBRATED[index][0])
                with self.subTest(index=index, mutation=mutation), self.assertRaises(ValueError):
                    self.verify(data)

    def test_ignored_changed_or_relocated_test_is_refused_even_with_updated_hash(self):
        original = self.path.read_bytes()
        for change in ('ignored', 'renamed', 'moved', 'body'):
            data = copy.deepcopy(self.data)
            body = original
            if change == 'ignored':
                body = body.replace(b'#[tokio::test(', b'#[ignore]\n#[tokio::test(', 1)
                for binding in data['bindings']:
                    binding['source_locations'][0]['line'] += 1
            elif change == 'renamed':
                body = body.replace(CALIBRATED[0][1].encode(), b'another_test', 1)
            elif change == 'moved':
                data['bindings'][0]['source_locations'][0]['path'] = 'other.rs'
            else:
                body += b'// changed source\n'
            self.path.write_bytes(body)
            if change != 'body':
                for binding in data['bindings']:
                    binding['source_locations'][0]['file_sha256'] = hashlib.sha256(body).hexdigest()
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.verify(data)
        self.path.write_bytes(original)

    def test_windows_crlf_checkout_preserves_the_canonical_source_binding(self):
        self.path.write_bytes(self.path.read_bytes().replace(b'\n', b'\r\n'))
        self.assertEqual(self.verify()[0]['historical_evidence'], 'E17')

    def test_retired_oracle_cannot_disappear_or_reappear_as_an_ignored_test(self):
        original = self.ledger.read_bytes()
        self.ledger.write_text('{"removed": {}}', encoding='utf-8')
        with self.assertRaises(ValueError):
            self.verify()
        self.ledger.write_bytes(original)
        self.old_source.write_text('#[ignore]\nasync fn stale_same_owner_claimer_is_refused_after_reclaim() {}', encoding='utf-8')
        with self.assertRaises(ValueError):
            self.verify()


class WorkStoreSelectionTests(unittest.TestCase):
    setUpClass = classmethod(WorkParitySelectionTests.setUpClass.__func__)
    verify = WorkParitySelectionTests.verify

    def changed_store(self, transform):
        start = self.source.index('\nwork_store_evidence() {') + 1
        end = self.source.index('\n}', start)
        original = self.source[start:end]
        changed = transform(original)
        self.assertNotEqual(original, changed)
        return self.source[:start] + changed + self.source[end:]

    def test_native_targets_preserve_the_complete_union_without_repeating_conformance(self):
        phases = self.verify(self.source)
        store = [p for p in phases if p.selector == 'work_store_evidence']
        self.assertEqual(len(store), 1)
        args = store[0].arguments
        targets = [args[i + 1] for i, value in enumerate(args) if value == '--test']
        self.assertEqual(targets, ['archive', 'environment_revision', 'lease_book_properties', 'work_identity', 'work_queue_parity', 'work_queue_properties'])
        self.assertIn('--lib', args)
        self.assertNotIn('work_queue_conformance', args)
        parity = [p for p in phases if p.selector == 'backend_parity' and p.package == 'awaken-work-store']
        self.assertEqual(len(parity), 1)
        self.assertIn('work_queue_conformance', parity[0].arguments)
        self.assertEqual(len(phases), 18)

    def test_missing_targets_and_broad_all_package_or_conformance_selection_are_refused(self):
        mutations = [lambda s: s.replace('--lib ', '', 1)]
        for target in ('archive', 'environment_revision', 'lease_book_properties', 'work_identity', 'work_queue_parity', 'work_queue_properties'):
            mutations.append(lambda s, target=target: s.replace('--test ' + target + ' ', '', 1))
        for added in ('--tests', '--all-targets', '--test work_queue_conformance'):
            mutations.append(lambda s, added=added: s.replace('--lib ', '--lib ' + added + ' ', 1))
        for mutation in mutations:
            with self.subTest(mutation=mutation), self.assertRaisesRegex(ValueError, 'independent Work store'):
                self.verify(self.changed_store(mutation))

    def test_each_required_outcome_and_original_private_helper_policy_are_mandatory(self):
        import re
        source = self.source.split('\nwork_store_evidence() {', 1)[1].split('\n}', 1)[0]
        floors = re.findall(r'--require-case=([^\s]+)', source)
        self.assertEqual(len(floors), 13)
        mutations = [lambda s, case=case: s.replace('--require-case=' + case, '', 1) for case in floors]
        mutations += [
            lambda s: s.replace('--include-ignored ', '', 1),
            lambda s: s.replace('--skip environment_revision_restart_child', '--skip environment_revision', 1),
            lambda s: s.replace('--skip restart::identity_restart_child', '--skip restart', 1),
            lambda s: s.replace('--test-threads=1', '--ignored --test-threads=1', 1),
        ]
        for mutation in mutations:
            with self.subTest(mutation=mutation), self.assertRaisesRegex(ValueError, 'independent Work store'):
                self.verify(self.changed_store(mutation))


    def test_all_live_work_callers_require_actual_metadata_and_propagate_failure(self):
        marker = 'python3 scripts/ci/_native_work_target_coverage.py --check || return "$?"'
        self.assertEqual(self.source.count(marker), 2)
        mutations = [
            self.source.replace(marker, 'true', 1),
            self.source.rsplit(marker, 1)[0] + 'true' + self.source.rsplit(marker, 1)[1],
            self.source.replace(marker, marker.replace('return "$?"', 'true'), 1),
            self.source.replace('\nchecked_work_store_evidence || record_failure "$?"\n', '\nwork_store_evidence\n', 1),
            self.source.replace('\nchecked_work_store_evidence || record_failure "$?"\n', '\nchecked_work_store_evidence\n', 1),
        ]
        for index, source in enumerate(mutations):
            self.assertNotEqual(source, self.source)
            with self.subTest(index=index), self.assertRaises(ValueError):
                self.verify(source)


if __name__ == '__main__':
    unittest.main(verbosity=2)
