"""Pure selector and ledger admission controls; never backend execution proof."""
import copy
from pathlib import Path
import tempfile
import unittest

from _native_postgres_gap_policy import (
    WORK_CURRENT_CASES, WORK_GAP_BINDINGS, WORK_PRODUCT_GAPS, WORK_SOURCE,
    postgres_product_gap_report, validate_work_product_gaps,
)
from _native_postgres_scopes import postgres_phases

ROOT = Path(__file__).resolve().parents[2]
EXPECTED_GAPS = (
    ('B247', 'T08-C02', 'retried_ack_replays_the_original_acknowledgement'),
    ('B248', 'T08-C02', 'retried_stop_after_lost_response_replays'),
    ('B250', 'T09-C02', 'stale_same_owner_claimer_is_refused_after_reclaim'),
    ('B251', 'T09-C02', 'ack_after_the_lease_lapsed_is_refused'),
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

    def test_all_current_cases_and_only_the_four_registered_gaps_are_declared(self):
        phases = self.verify(self.source)
        self.assertEqual(len(phases), 18)
        work = [p for p in phases if p.selector == 'backend_parity' and p.package == 'awaken-work-store']
        self.assertEqual(len(work), 1)
        self.assertEqual(len(WORK_CURRENT_CASES), 10)
        self.assertEqual(WORK_GAP_BINDINGS, EXPECTED_GAPS)
        self.assertEqual(WORK_PRODUCT_GAPS, tuple('lease_acceptance::' + row[2] for row in EXPECTED_GAPS))
        floors = [a.removeprefix('--require-case=') for a in work[0].arguments if a.startswith('--require-case=')]
        self.assertEqual(tuple(floors), WORK_CURRENT_CASES)
        self.assertNotIn('--ignored', work[0].arguments)

    def test_each_current_case_is_mandatory_and_cannot_be_replaced(self):
        for case in WORK_CURRENT_CASES:
            for replacement in ('', '--require-case=unrelated'):
                changed = self.changed_work(lambda text: text.replace('--require-case=' + case, replacement, 1))
                with self.subTest(case=case, replacement=replacement), self.assertRaisesRegex(ValueError, 'independent Work'):
                    self.verify(changed)

    def test_each_gap_exclusion_must_remain_exact_and_cannot_expand(self):
        for case in WORK_PRODUCT_GAPS:
            for replacement in ('', '--skip lease_acceptance', '--skip in_memory_backend_conforms'):
                changed = self.changed_work(lambda text: text.replace('--skip ' + case, replacement, 1))
                with self.subTest(case=case, replacement=replacement), self.assertRaisesRegex(ValueError, 'independent Work'):
                    self.verify(changed)

    def test_target_features_filter_and_ignored_flags_cannot_change_current_scope(self):
        mutations = (
            lambda text: text.replace('--test work_queue_conformance', '--test other_target', 1),
            lambda text: text.replace('--features test-support', '--features other', 1),
            lambda text: text.replace('    -- \\\n', '    other_filter -- \\\n', 1),
            lambda text: text.replace('--test-threads=1', '--ignored --test-threads=1', 1),
            lambda text: text.replace('--test-threads=1', '--include-ignored --test-threads=1', 1),
            lambda text: text.replace('--skip ' + WORK_PRODUCT_GAPS[0], '--skip=' + WORK_PRODUCT_GAPS[0], 1),
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
        import check_rust_test_execution as execution
        inventory = 'current: test\nunknown_new_gap: test\n\n2 tests, 0 benchmarks\n'
        results = (
            'running 2 tests\n'
            'test current ... ok\n'
            'test unknown_new_gap ... ignored, product gap: new\n'
            'test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n'
        )
        observed = execution.execution_evidence(inventory, results, 0, ['current'])
        self.assertTrue(observed['errors'])


class WorkGapAdmissionTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.path = self.root / WORK_SOURCE
        self.path.parent.mkdir(parents=True)
        lines, bindings = [], []
        for binding_id, case_id, name in EXPECTED_GAPS:
            lines += ['#[tokio::test]', '#[ignore = "product gap: required invariant"]',
                      'async fn ' + name + '() { assert!(false); }', '']
            bindings.append({
                'id': binding_id, 'name': name, 'relation': 'reproduces_product_gap',
                'execution_witnesses': [],
                'source_locations': [{'path': WORK_SOURCE, 'line': len(lines) - 1}],
                'covers': {case_id: {'oracles': [], 'negative_control': False}},
            })
        self.path.write_text('\n'.join(lines) + '\n', encoding='utf-8')
        cases = [{
            'id': case_id, 'evidence_state': 'product_gap',
            'binding_ids': [row[0] for row in EXPECTED_GAPS if row[1] == case_id],
            'product_gap': {
                'reason': 'unaccepted invariant', 'follow_up': 'separate protocol work',
                'reproducers': [row[2] for row in EXPECTED_GAPS if row[1] == case_id],
            },
        } for case_id in ('T08-C02', 'T09-C02')]
        self.data = {'groups': [{'cases': cases}], 'bindings': bindings}

    def verify(self, data=None):
        return validate_work_product_gaps(self.data if data is None else data, self.root)

    def test_report_keeps_every_gap_unverified_without_an_execution_witness(self):
        report = self.verify()
        self.assertEqual(len(report), 4)
        self.assertEqual([r['binding'] for r in report], [row[0] for row in EXPECTED_GAPS])
        self.assertTrue(all(r['status'] == 'unverified-product-gap' for r in report))
        self.assertTrue(all(r['reason'] and r['follow_up'] for r in report))
        self.assertEqual(postgres_product_gap_report('resources', self.root), [])
        self.assertEqual(postgres_product_gap_report('storage', self.root), [])
        with self.assertRaises(ValueError):
            postgres_product_gap_report('unknown', self.root)

    def test_missing_duplicate_or_unrelated_bindings_are_refused(self):
        missing = copy.deepcopy(self.data)
        missing['bindings'].pop()
        duplicated = copy.deepcopy(self.data)
        duplicated['bindings'].append(copy.deepcopy(duplicated['bindings'][0]))
        unrelated = copy.deepcopy(self.data)
        unrelated['bindings'][0]['name'] = 'another_case'
        wrong_path = copy.deepcopy(self.data)
        wrong_path['bindings'][0]['source_locations'][0]['path'] = 'other.rs'
        for data in (missing, duplicated, unrelated, wrong_path):
            with self.subTest(data=data), self.assertRaises(ValueError):
                self.verify(data)

    def test_promoted_unreasoned_or_expanded_gap_classification_requires_review(self):
        for field, value in (('evidence_state', 'current_subset'), ('evidence_state', 'current_clause_complete')):
            data = copy.deepcopy(self.data)
            data['groups'][0]['cases'][0][field] = value
            with self.subTest(value=value), self.assertRaises(ValueError):
                self.verify(data)
        for field in ('reason', 'follow_up', 'reproducers'):
            data = copy.deepcopy(self.data)
            data['groups'][0]['cases'][0]['product_gap'].pop(field)
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.verify(data)
        data = copy.deepcopy(self.data)
        data['groups'][0]['cases'][0]['product_gap']['reproducers'].append('another_gap')
        with self.assertRaises(ValueError):
            self.verify(data)

    def test_a_gap_cannot_become_execution_evidence_or_change_its_acceptance_case(self):
        for mutation in ('witness', 'relation', 'case', 'binding', 'oracle'):
            data = copy.deepcopy(self.data)
            binding = data['bindings'][0]
            if mutation == 'witness':
                binding['execution_witnesses'] = [{'status': 'passed'}]
            elif mutation == 'relation':
                binding['relation'] = 'proves_clause'
            elif mutation == 'case':
                binding['covers'] = {'T00-C01': {}}
            elif mutation == 'oracle':
                binding['covers']['T08-C02']['oracles'] = [0]
            else:
                data['groups'][0]['cases'][0]['binding_ids'].remove('B247')
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                self.verify(data)

    def test_unignored_moved_or_missing_reproducer_source_requires_review(self):
        original = self.path.read_text(encoding='utf-8')
        self.path.write_text(original.replace('#[ignore = "product gap: required invariant"]', '// no longer ignored', 1))
        with self.assertRaises(ValueError):
            self.verify()
        self.path.write_text(original.replace('async fn ' + EXPECTED_GAPS[0][2], 'async fn another_case', 1))
        with self.assertRaises(ValueError):
            self.verify()
        self.path.write_text(original)
        data = copy.deepcopy(self.data)
        data['bindings'][0]['source_locations'][0]['line'] += 1
        with self.assertRaises(ValueError):
            self.verify(data)
        self.path.unlink()
        with self.assertRaises((ValueError, FileNotFoundError)):
            self.verify()


if __name__ == '__main__':
    unittest.main(verbosity=2)
