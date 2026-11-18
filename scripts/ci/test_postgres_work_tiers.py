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
from _postgres_phase_plan import edit_row, row_block

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
        cls.table = (ROOT / 'scripts/ci/service_suites.toml').read_text(encoding='utf-8')

    def verify(self, source=None, table=None, scope="core"):
        # The suite rows live in the table; pg_tests.sh keeps the checked wrappers.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = root / 'scripts/ci/pg_tests.sh'
            script.parent.mkdir(parents=True)
            script.write_text(self.source if source is None else source, encoding='utf-8')
            (root / 'scripts/ci/service_suites.toml').write_text(self.table if table is None else table, encoding='utf-8')
            return postgres_phases(scope, root)

    def changed_work(self, transform):
        return edit_row(self.table, 'postgres.parity.work-store', transform)

    def test_all_current_cases_are_required_without_retired_gap_exclusions(self):
        phases = self.verify(self.source)
        self.assertEqual(len(phases), 23)
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
                    self.verify(table=changed)

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
                self.verify(table=changed)


    def test_no_current_case_can_be_skipped_and_retired_oracle_cannot_return_as_skip(self):
        for case in (*WORK_CURRENT_CASES, 'lease_acceptance::stale_same_owner_claimer_is_refused_after_reclaim'):
            changed = self.changed_work(lambda text: text.replace('--test-threads=1',
                '--skip ' + case + ' --test-threads=1', 1))
            with self.subTest(case=case), self.assertRaisesRegex(ValueError, 'independent Work'):
                self.verify(table=changed)

    def test_target_features_filter_and_ignored_flags_cannot_change_current_scope(self):
        mutations = (
            lambda text: text.replace('--test work_queue_conformance', '--test other_target', 1),
            lambda text: text.replace('--features test-support', '--features other', 1),
            lambda text: text.replace(' -- ', ' other_filter -- ', 1),
            lambda text: text.replace('--test-threads=1', '--ignored --test-threads=1', 1),
            lambda text: text.replace('--test-threads=1', '--include-ignored --test-threads=1', 1),
            lambda text: text.replace('--test-threads=1', '--skip=lease_acceptance --test-threads=1', 1),
        )
        for index, mutation in enumerate(mutations):
            with self.subTest(index=index), self.assertRaisesRegex(ValueError, 'independent Work'):
                self.verify(table=self.changed_work(mutation))

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
                'source_locations': [{'path': WORK_SOURCE, 'function': name}],
                'covers': {'T09-C02': {'oracles': oracles, 'negative_control': False}}})
        self.path.write_bytes(('\n'.join(lines) + '\n').encode())
        for binding in bindings:
            binding['source_locations'][0]['file_sha256'] = hashlib.sha256(self.path.read_bytes()).hexdigest()
            from check_session_acceptance import _rust_test_identity
            binding['source_locations'][0]['function_sha256'] = _rust_test_identity(
                self.path.read_text(encoding='utf-8'), binding['name'])[0]
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
        for change in ('ignored', 'renamed', 'moved', 'body', 'signature'):
            data = copy.deepcopy(self.data)
            body = original
            if change == 'ignored':
                body = body.replace(b'#[tokio::test(', b'#[ignore]\n#[tokio::test(', 1)
            elif change == 'renamed':
                body = body.replace(CALIBRATED[0][1].encode(), b'another_test', 1)
            elif change == 'moved':
                data['bindings'][0]['source_locations'][0]['path'] = 'other.rs'
            elif change == 'signature':
                body = body.replace(b'async fn ', b'fn ', 1)
            else:
                body = body.replace(b'assert!(true)', b'assert!(false)', 1)
            self.path.write_bytes(body)
            if change != 'body':
                for binding in data['bindings']:
                    binding['source_locations'][0]['file_sha256'] = hashlib.sha256(body).hexdigest()
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.verify(data)
        self.path.write_bytes(original)

    def test_source_motion_comments_and_formatting_preserve_the_calibrated_function(self):
        original = self.path.read_bytes()
        for body in (b'// inserted before the tests\n\n' + original,
                     original + b'// unrelated source after the tests\n',
                     original.replace(b'assert!(true)', b'assert!( /* format only */ true )')):
            with self.subTest(body=body[:32]):
                self.path.write_bytes(body)
                self.assertEqual(self.verify()[0]['historical_evidence'], 'E17')
        self.path.write_bytes(original)

    def test_ignore_attributes_remain_rejected_across_comments_and_attribute_layout(self):
        original = self.path.read_bytes()
        for attribute in (b'#[ignore]', b'#[cfg_attr(test, ignore)]'):
            body = attribute + b'\n' + b'// unrelated\n' * 12 + original
            self.path.write_bytes(body)
            with self.subTest(attribute=attribute), self.assertRaises(ValueError):
                self.verify()
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
        return edit_row(self.table, 'postgres.work.work-store', transform)

    def test_native_targets_preserve_the_complete_union_without_repeating_conformance(self):
        phases = self.verify()
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
        self.assertEqual(len(phases), 23)

    def test_missing_targets_and_broad_all_package_or_conformance_selection_are_refused(self):
        mutations = [lambda s: s.replace('--lib ', '', 1)]
        for target in ('archive', 'environment_revision', 'lease_book_properties', 'work_identity', 'work_queue_parity', 'work_queue_properties'):
            mutations.append(lambda s, target=target: s.replace('--test ' + target + ' ', '', 1))
        for added in ('--tests', '--all-targets', '--test work_queue_conformance'):
            mutations.append(lambda s, added=added: s.replace('--lib ', '--lib ' + added + ' ', 1))
        for mutation in mutations:
            with self.subTest(mutation=mutation), self.assertRaisesRegex(ValueError, 'independent Work store'):
                self.verify(table=self.changed_store(mutation))

    def test_each_required_outcome_and_original_private_helper_policy_are_mandatory(self):
        import re
        source = self.table.split('id = "postgres.work.work-store"', 1)[1].split('[[', 1)[0]
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
                self.verify(table=self.changed_store(mutation))


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


class SessionWorkReleaseSelectionTests(unittest.TestCase):
    setUpClass = classmethod(WorkParitySelectionTests.setUpClass.__func__)
    verify = WorkParitySelectionTests.verify
    # Independent literals, not derived from the executable table or policy.
    cases = (
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
        'session_work_release::tests::http::live_postgres_http_old_work_receipt_cannot_renew_a_same_owner_successor',
        'session_work_release::tests::http::live_postgres_http_legacy_checkpoint_null_rechecks_exact_claim',
        'session_work_release::tests::http::settlement::live_postgres_http_cap_one_settles_real_memory_once_without_held_guard',
        'session_work_release::tests::http::races::live_postgres_http_same_claim_cold_absence_recovers_the_one_accepted_receipt',
        'session_work_release::tests::http::races::live_postgres_http_cold_resume_takeover_refuses_before_claim_specific_root_write',
        'session_work_release::tests::http::settlement::live_postgres_http_memory_observer_release_preserves_a_successor_before_settlement_cas',
        'session_work_release::tests::http::preparation::live_postgres_http_claimed_preparation_commits_once_and_replays_original_receipt',
        'session_work_release::tests::http::preparation::live_postgres_http_unkeyed_credential_no_change_keeps_the_current_root',
        'session_work_release::tests::http::preparation::live_postgres_http_slow_issuer_rechecks_claim_work_expiry_and_physical_slot',
        'session_work_release::tests::http::preparation::live_postgres_http_credential_commit_and_no_change_read_keep_the_final_claim_gate',
        'session_work_release::tests::http::preparation::live_postgres_http_issuer_failure_preserves_the_same_claim_for_retry',
        'session_work_release::tests::http::preparation::live_postgres_http_aborted_preparation_returns_cap_one_for_the_same_http_retry',
        'session_work_release::tests::http::preparation_work::live_postgres_http_preparation_handoff_rechecks_canonical_root_worker_and_profile_after_work',
    )

    def test_borrowed_root_cases_stay_in_the_original_storage_phase(self):
        # R5 the nested SQL-kernel cases must match postgres_ and be required,
        # never merely listed by an unrelated whole-lib or ignored suite.
        cases = (
            'postgres::mutation::tests::postgres_borrowed_commit_keeps_caller_transaction_and_rolls_back_late_sql_failure',
            'postgres::mutation::tests::postgres_borrowed_commit_replays_exact_receipt_after_lost_acknowledgement',
            'postgres::mutation::tests::postgres_borrowed_commit_preserves_private_binding_noop_and_delete_receipts',
        )
        phases = self.verify(scope="storage")
        phase = next(p for p in phases if p.package == "awaken-session-store")
        for case in cases:
            self.assertIn("--require-case=" + case, phase.arguments)
            self.assertIn("postgres_", case)
            for replacement in ("", "--require-case=unrelated"):
                table = edit_row(self.table, "postgres.storage.session-store", lambda text: text.replace("--require-case=" + case, replacement, 1))
                with self.subTest(case=case), self.assertRaisesRegex(ValueError, "independent Session extraction outcomes"):
                    self.verify(table=table, scope="storage")

    def test_outbox_bootstrap_cases_stay_in_the_original_storage_phase(self):
        # Each real nested case must execute through the existing postgres_
        # Session store row. Literal names form an independent floor oracle.
        cases = (
            'tests::outbox_bootstrap::postgres_outbox_index_gate_waiters_do_not_deadlock_online_build',
            'tests::outbox_bootstrap::postgres_outbox_index_gate_waiter_abort_restores_single_connection_pool',
            'tests::outbox_bootstrap::postgres_outbox_index_builder_abort_and_error_release_gate_for_retry',
        )
        phase = next(p for p in self.verify(scope="storage")
                     if p.package == "awaken-session-store")
        self.assertIn("postgres_", phase.arguments)
        for case in cases:
            self.assertIn("--require-case=" + case, phase.arguments)
            for replacement in ("", "--require-case=unrelated"):
                table = edit_row(
                    self.table, "postgres.storage.session-store",
                    lambda text: text.replace("--require-case=" + case, replacement, 1),
                )
                self.assertNotEqual(table, self.table)
                with self.subTest(case=case, replacement=replacement), self.assertRaisesRegex(
                    ValueError, "independent Session extraction outcomes"
                ):
                    self.verify(table=table, scope="storage")
            table = edit_row(
                self.table, "postgres.storage.session-store",
                lambda text: text.replace("--test-threads=1", "--skip " + case + " --test-threads=1", 1),
            )
            self.assertNotEqual(table, self.table)
            with self.subTest(case=case, skipped=True), self.assertRaisesRegex(
                ValueError, "independent Session extraction outcomes"
            ):
                self.verify(table=table, scope="storage")

    def changed_release(self, transform):
        return edit_row(self.table, 'postgres.critical.process-startup-work-release', transform)

    def test_additive_release_and_http_floor_preserves_the_original_eighteen_domains(self):
        # R1 original 18 + startup Work + three exact claimed rows + lifecycle Outbox => admit 23; all
        # twenty-one independently named outcomes. The old binding/pool rows remain.
        phases = self.verify()
        self.assertEqual(len(phases), 23)
        startup = [p for p in phases if p.package == 'awaken-process-startup']
        self.assertEqual(len(startup), 3)
        for original in (
            'installation_binding::tests::postgres_preflights_all_targets_then_binds_fresh_or_explicit_legacy',
            'postgres_pools::tests::live_coordinator_adapters_share_the_process_connection_ceiling',
        ):
            self.assertTrue(any('--require-case=' + original in p.arguments for p in startup))
        selected = [p for p in startup if 'session_work_release::tests::' in p.arguments]
        self.assertEqual(len(selected), 1)
        actual = tuple(a.removeprefix('--require-case=') for a in selected[0].arguments
                       if a.startswith('--require-case='))
        self.assertEqual(actual, self.cases)

    def test_every_new_outcome_is_independent_and_cannot_be_deleted_replaced_or_skipped(self):
        # R2 missing/replaced any floor or skipping its real execution => refuse;
        # another startup outcome cannot silently substitute for that case.
        for case in self.cases:
            existing = self.cases[1] if case == self.cases[0] else self.cases[0]
            for mutation in (
                lambda s: s.replace('--require-case=' + case, '', 1),
                lambda s: s.replace('--require-case=' + case, '--require-case=unrelated', 1),
                lambda s: s.replace('--require-case=' + case, '--require-case=' + existing, 1),
                lambda s: s.replace('--require-case=' + case, '--require-case=' + case + ' --require-case=' + case, 1),
                lambda s: s.replace('--test-threads=1', '--skip ' + case + ' --test-threads=1', 1),
            ):
                table = self.changed_release(mutation)
                self.assertNotEqual(table, self.table, "the negative control must change the actual authored row")
                with self.subTest(case=case, mutation=mutation), self.assertRaisesRegex(ValueError, 'independent Session Work release'):
                    self.verify(table=table)

    def test_new_scope_cannot_broaden_or_hide_ignored_cases(self):
        # R3 wrong lib/filter/runtime mode/thread policy => refuse, even while
        # all independently named outcomes are present. R4 another package cannot supply this row.
        mutations = (
            lambda s: s.replace('--lib', '--tests', 1),
            lambda s: s.replace('--lib', '--lib --all-targets', 1),
            lambda s: s.replace('session_work_release::tests:: ', 'some_other_filter ', 1),
            lambda s: s.replace('--ignored', '--include-ignored', 1),
            lambda s: s.replace('--ignored', '--ignored --exact', 1),
            lambda s: s.replace('--test-threads=1', '--test-threads=2', 1),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation), self.assertRaisesRegex(ValueError, 'independent Session Work release'):
                self.verify(table=self.changed_release(mutation))
        with self.assertRaisesRegex(ValueError, 'independent required domain coverage'):
            self.verify(table=self.changed_release(lambda s: s.replace('awaken-process-startup', 'awaken-coordinator', 1)))


class ClaimedCommitSelectionTests(unittest.TestCase):
    setUpClass = classmethod(WorkParitySelectionTests.setUpClass.__func__)
    verify = WorkParitySelectionTests.verify
    # Independent policy oracles: do not derive required evidence from the
    # executable suite table or from the production selector's own floor list.
    rows = (
        ('postgres.critical.claimed-commit-pool', (
            'runtime_authority::claimed_commit_tests::pool_tests::claimed_commit_shared_pool_cap_one_progresses',
            'runtime_authority::claimed_commit_tests::pool_tests::claimed_commit_shared_pool_cap_two_concurrent_progresses',
            'runtime_authority::claimed_commit_tests::pool_tests::claimed_postgres_cold_child_scope_replay_and_ordinary_binding',
            'runtime_authority::claimed_commit_tests::pool_tests::claimed_postgres_rollback_preserves_projection_and_receipt',
        )),
        ('postgres.critical.claimed-child-recovery', (
            'agent_runner::child_execution::tests::local_postgres_child_permission_recovers_with_its_own_prefix',
        )),
        ('postgres.critical.coordinator-claimed-writer', (
            'runtime_authority::claimed_commit_tests::selected_postgres_writer_rolls_back_late_failure_and_replays_on_one_connection',
            'runtime_authority::claimed_commit_tests::selected_postgres_writer_serializes_commit_and_takeover',
            'runtime_authority::claimed_commit_tests::selected_postgres_writer_fences_after_waiting_for_a_new_epoch',
            'runtime_authority::claimed_commit_tests::selected_postgres_writer_rolls_back_when_the_commit_connection_dies',
            'runtime_authority::claimed_commit_tests::selected_postgres_cold_reads_freeze_pages_and_resume_lifecycle_after_takeover',
        )),
        ('postgres.critical.coordinator', (
            'application_access_store::tests::provisioned_postgres_application_access_durability_release_gate',
        )),
    )

    def test_exact_pool_child_and_selected_writer_outcomes_add_to_every_existing_domain(self):
        # C1 all old domains remain; C2 five pool/child and five selected-Postgres
        # outcomes are declared on their exact targets alongside lifecycle Outbox => admit 23 domains.
        # Constraint: the five ordinary selected-Postgres cases have their own
        # row beside ignored pool/child and original Application Access. All prior
        # Work/Dispatch observations remain independently required.
        phases = self.verify()
        self.assertEqual(len(phases), 23)
        for row, cases in self.rows:
            block = self.table.split('id = "' + row + '"', 1)[1].split('[[', 1)[0]
            for case in cases:
                self.assertIn('--require-case=' + case, block)
        observations = [p for p in phases if p.package == 'awaken-dispatch'
                        and p.selector == 'backend_parity']
        self.assertEqual(len(observations), 1)
        self.assertIn('--require-case=postgres_exact_physical_execution_observations_use_the_original_store_clock', observations[0].arguments)

    def test_every_live_outcome_refuses_deletion_replacement_or_runtime_skip(self):
        # R1 any missing/renamed floor or excluded execution => refusal, even
        # when every other named case and all previous domains remain present.
        for row, cases in self.rows:
            for case in cases:
                for index, mutation in enumerate((
                    lambda s: s.replace('--require-case=' + case, '', 1),
                    lambda s: s.replace('--require-case=' + case, '--require-case=unrelated', 1),
                    lambda s: s.replace('--test-threads=1', '--skip ' + case + ' --test-threads=1', 1),
                )):
                    # The one-case row loses its entire floor at the table
                    # admission owner before the domain selector can run.
                    error = ('nonempty required case floors are mandatory'
                             if len(cases) == 1 and index == 0 else 'independent claimed-commit')
                    with self.subTest(row=row, case=case), self.assertRaisesRegex(ValueError, error):
                        self.verify(table=edit_row(self.table, row, mutation))

    def test_target_scope_ignored_execution_and_domain_identity_stay_exact(self):
        # R2 broad target/runtime mode/thread policy or another package cannot
        # substitute for the causal pool, child recovery or selected-writer
        # cases. R3 adding ignored/exact to the ordinary prefix would execute
        # none of its five witnesses; changing their target/filter also refuses.
        for row, _ in self.rows:
            mutations = [
                lambda s: s.replace('--locked', '--locked --all-targets', 1),
                lambda s: s.replace('--test-threads=1', '--test-threads=2', 1),
            ]
            if row == 'postgres.critical.coordinator-claimed-writer':
                mutations.extend((
                    lambda s: s.replace('--test-threads=1', '--ignored --test-threads=1', 1),
                    lambda s: s.replace('--test-threads=1', '--exact --test-threads=1', 1),
                    lambda s: s.replace('--lib ', '--test other_target ', 1),
                    lambda s: s.replace('selected_postgres_ ', 'another_prefix ', 1),
                    # The old four-writer prefix silently omits the cold-page
                    # witness despite declaring its floor: reject that narrowing.
                    lambda s: s.replace('selected_postgres_ ', 'selected_postgres_writer_ ', 1),
                ))
            else:
                mutations.append(lambda s: s.replace('--ignored', '--include-ignored', 1))
                if row == 'postgres.critical.claimed-commit-pool':
                    mutations.extend((
                        lambda s: s.replace('--lib ', '--test other_target ', 1),
                        lambda s: s.replace('pool_tests:: ', 'other_prefix:: ', 1),
                        lambda s: s.replace('--ignored', '--ignored --exact', 1),
                    ))
            for mutation in mutations:
                with self.subTest(row=row), self.assertRaisesRegex(ValueError, 'independent claimed-commit'):
                    self.verify(table=edit_row(self.table, row, mutation))
        for row, original in (
            ('postgres.critical.claimed-commit-pool', 'awaken-coordinator'),
            ('postgres.critical.claimed-child-recovery', 'awaken-session-executor'),
            ('postgres.critical.coordinator-claimed-writer', 'awaken-coordinator'),
            ('postgres.critical.coordinator', 'awaken-coordinator'),
        ):
            with self.subTest(row=row), self.assertRaisesRegex(ValueError, 'independent required domain coverage'):
                replacement = 'awaken-dispatch' if original == 'awaken-coordinator' else 'awaken-coordinator'
                self.verify(table=edit_row(self.table, row, lambda s: s.replace(original, replacement, 1)))

    def test_coordinator_rows_cannot_replace_one_another_with_the_same_package_count(self):
        # R4 removal loses one independent domain. R5 each same-package copy
        # preserves the Counter yet must refuse: pool, selected-writer and
        # Application Access receipts each prove different original effects.
        rows = ('postgres.critical.claimed-commit-pool',
                'postgres.critical.coordinator-claimed-writer', 'postgres.critical.coordinator')
        for removed in rows:
            original = row_block(self.table, removed)
            with self.subTest(row=removed, rule='missing'), self.assertRaisesRegex(ValueError, 'independent required domain coverage'):
                self.verify(table=self.table.replace(original, '', 1))
            for other in rows:
                if other == removed:
                    continue
                replacement = row_block(self.table, other).replace(
                    'id = "' + other + '"', 'id = "' + removed + '"', 1)
                with self.subTest(row=removed, substitute=other), self.assertRaisesRegex(ValueError, 'independent claimed-commit'):
                    self.verify(table=self.table.replace(original, replacement, 1))


if __name__ == '__main__':
    unittest.main(verbosity=2)
