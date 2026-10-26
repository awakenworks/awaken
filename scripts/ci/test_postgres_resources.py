#!/usr/bin/env python3
"""Resource selector and exact live-fixture receipt rejection laws."""
import contextlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import check_native_services as gate
from _native_service_receipt_fixture import FIXTURE_OWNER, postgres_receipts
from _native_postgres_scopes import postgres_plan
import _native_postgres_scopes as policy

INPUT_GLOBS = ("scripts/ci/**", ".github/workflows/windows-native-services.yml")

PACKAGES=('awaken-admin-config-api','awaken-resource-store','awaken-resource-persistence',
    'awaken-session-application','awaken-protocol-managed','awaken-sandbox-policy-store',
    'awaken-executable-agent-catalog','awaken-executable-environment-catalog','awaken-environment-application', 'awaken-environment-image-build')


# Independent original application outcomes; never derive these from the selector.
EXPECTED_APPLICATION_CASES = frozenset({
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
ORIGINAL_MISSING_FLOORS = (
    'tests::authority::archive_race::postgres_archive_races_admission_replacement_and_recovery_through_root_cas',
    'tests::authority::archive_race::postgres_archive_races_admission_replacement_and_recovery_through_root_cas_fixture_admission',
    'tests::creation::prepared_participants::postgres_prepared_participant_survives_root_failure_and_rebuild',
    'tests::creation::prepared_participants::postgres_prepared_participant_survives_root_failure_and_rebuild_fixture_admission',
    'tests::event_batches::concurrent_consumption::postgres_independent_clients_consume_one_tool_ticket_identically_to_sqlite',
    'tests::event_batches::concurrent_consumption::postgres_independent_clients_consume_one_tool_ticket_identically_to_sqlite_fixture_admission',
)

class ResourceSelectionTests(unittest.TestCase):
    def selection(self,status):
        source=(gate.ROOT/'scripts/ci/pg_tests.sh').read_text(encoding='utf-8')
        def function(name):
            match=re.search(r'(?ms)^'+name+r'\(\) \{\n.*?^\}',source)
            self.assertIsNotNone(match,name);return match.group(0)
        with tempfile.TemporaryDirectory(prefix='resource selector ') as directory:
            root=Path(directory);calls=root/'calls';script=root/'selection.sh'
            script.write_text('set -uo pipefail\nfirst_failure=0\n'
                'pg_required_test(){ printf "%s\\0" "$@" >>"$CALLS"; printf "\\n" >>"$CALLS"; return "$RUNNER_STATUS"; }\n'
                +function('record_failure')+'\n'+function('resources_evidence')+'\nresources_evidence\nexit "$first_failure"\n',encoding='utf-8',newline='\n')
            child=subprocess.run([sys.executable,str(gate.ROOT/'scripts/ci/_native_shell.py'),script.as_posix()],
                env=dict(os.environ,CALLS=calls.as_posix(),RUNNER_STATUS=str(status)),capture_output=True,timeout=30)
            return child.returncode,[row.decode().split('\0')[:-1] for row in calls.read_bytes().split(b'\n') if row]

    def test_all_ten_domains_keep_features_named_floor_and_new_admission_laws(self):
        code,rows=self.selection(0);self.assertEqual(code,0)
        self.assertEqual([row[row.index('-p')+1] for row in rows],list(PACKAGES))
        for row in rows:
            self.assertIn('--locked',row);self.assertIn('--test-threads=1',row)
            self.assertTrue(any(x.startswith('--require-case=') for x in row[:row.index('--')]))
        self.assertEqual(rows[0][rows[0].index('--features')+1],'postgres')
        self.assertIn('--all-features',rows[1]);self.assertIn('--exact',rows[-1])
        counts=[sum(x.endswith('_fixture_admission') for x in row if x.startswith('--require-case=')) for row in rows]
        self.assertEqual(counts,[0, 4, 3, 6, 1, 0, 0, 1, 5, 0])
        # The two filtered groups must include the original domain and its new
        # admission case. An exact original-only filter would erase the latter.
        self.assertNotIn('--exact',rows[3]);self.assertNotIn('--exact',rows[4])

    def test_environment_replay_and_countermodel_floors_are_required(self):
        code,rows=self.selection(0);self.assertEqual(code,0)
        replay=next(row for row in rows if row[row.index('-p')+1]=='awaken-environment-application')
        self.assertEqual(replay[replay.index('--test')+1],'registration_replay')
        selected={value.removeprefix('--require-case=') for value in replay if value.startswith('--require-case=')}
        required={
            'durable_pending::a_catalog_high_watermark_projection_is_rejected_by_the_convergence_verdict',
            'durable_pending::postgres_history_cannot_consume_unapplied_higher_registration_fixture_admission',
            'durable_pending::postgres_pending_history_does_not_swallow_new_registration_effect_fixture_admission',
            'durable_pending::postgres_pending_history_survives_a_rebuild_through_independent_pools',
            'durable_pending::postgres_pending_history_survives_a_rebuild_through_independent_pools_fixture_admission',
            'history_visibility::a_non_durable_reconnect_cannot_pass_the_delayed_registration_check',
            'history_visibility::postgres_delayed_registration_after_reconnect_cannot_restore_withdrawn_work_fixture_admission',
            'history_visibility::postgres_withdrawn_history_stays_readable_on_independent_pools',
            'history_visibility::postgres_withdrawn_history_stays_readable_on_independent_pools_fixture_admission',
            'history_visibility::unrelated_faults_never_qualify_lost_withdrawal_detection',
            'postgres_delayed_registration_after_reconnect_cannot_restore_withdrawn_work',
            'postgres_history_cannot_consume_unapplied_higher_registration',
            'postgres_pending_history_does_not_swallow_new_registration_effect',
        }
        self.assertTrue(required<=selected,required-selected)

    def test_execution_failure_and_timeout_preserve_first_status_and_other_domains(self):
        for status in (23,124):
            with self.subTest(status=status):
                code,rows=self.selection(status);self.assertEqual(code,status);self.assertEqual(len(rows),10)

    def test_resource_refusal_does_not_start_the_next_domain(self):
        code,rows=self.selection(75);self.assertEqual(code,75);self.assertEqual(len(rows),1)

    def test_native_plan_is_one_resource_owner_and_retains_storage_and_core(self):
        self.assertEqual(postgres_plan('resources'),[('postgres-resources','resources.log',['scripts/ci/pg_tests.sh','--resources'])])
        self.assertEqual(len(postgres_plan('core')),4);self.assertEqual(len(postgres_plan('storage')),1)

    def test_non_postgres_service_refuses_scope_before_launch(self):
        for name in ('nats','s3'):
            with self.subTest(service=name),contextlib.redirect_stderr(io.StringIO()),mock.patch.object(gate,'worker') as worker:
                with self.assertRaises(SystemExit) as error:gate.main(['--service',name,'--postgres-scope','resources','--output','not-created'])
                self.assertEqual(error.exception.code,2);worker.assert_not_called()

    def test_native_workflow_requires_resource_scope(self):
        source=(gate.ROOT/'.github/workflows/windows-native-services.yml').read_text(encoding='utf-8')
        self.assertIn('--service postgres --postgres-scope resources --postgres-bin',source)

    def test_receipt_faults_cannot_hide_missing_or_wrong_resource_executions(self):
        import check_rust_test_execution as execution
        with tempfile.TemporaryDirectory() as directory,mock.patch.object(execution,'verify_preparation'):
            base=Path(directory);paths=postgres_receipts(base,'resources')
            def log(paths):return ''.join('execution evidence: '+str(path)+'\n' for path in paths)
            with mock.patch.dict(os.environ,{'CARGO_TARGET_DIR':str(base)}):
                self.assertEqual(len(gate.reconcile_postgres(log(paths),'source','resources',fixture_owner=FIXTURE_OWNER)),10)
                for rows in (paths[:-1],[*paths[:-1],paths[-2]]):
                    with self.assertRaises(ValueError):gate.reconcile_postgres(log(rows),'source','resources',fixture_owner=FIXTURE_OWNER)
                with self.assertRaises(ValueError):gate.reconcile_postgres(log(paths),'wrong-source','resources',fixture_owner=FIXTURE_OWNER)
                path=paths[0]/'results.json';original=json.loads(path.read_text())
                for change in ({'arguments':['-p','awaken-memory-store']},{'postgres_fixture_required':False},{'postgres_fixture_configured':False}):
                    path.write_text(json.dumps(dict(original,**change)),encoding='utf-8')
                    with self.subTest(change=change),self.assertRaises(ValueError):gate.reconcile_postgres(log(paths),'source','resources',fixture_owner=FIXTURE_OWNER)
                path.write_text(json.dumps(original),encoding='utf-8')
                path=paths[0]/'results.log';original=path.read_text()
                for output in (original.replace(' ... ok',' ... ignored'),original+'SKIPPED PostgreSQL fixture\n'):
                    path.write_text(output,encoding='utf-8')
                    with self.assertRaises(ValueError):gate.reconcile_postgres(log(paths),'source','resources',fixture_owner=FIXTURE_OWNER)

class ApplicationCaseFloorTests(unittest.TestCase):

    def setUp(self):
        self.source = (gate.ROOT / 'scripts/ci/pg_tests.sh').read_text(encoding='utf-8')

    def check(self, text):
        with tempfile.TemporaryDirectory(prefix='pg-case-policy-') as t:
            d = Path(t) / 'scripts/ci'
            d.mkdir(parents=True)
            (d / 'pg_tests.sh').write_text(text)
            return policy.postgres_phases('resources', Path(t))

    def test_complete_existing_case_set_keeps_all_ten_resource_domains(self):
        self.assertEqual(policy.RESOURCE_APPLICATION_CASE_FLOORS, EXPECTED_APPLICATION_CASES)
        selected = self.check(self.source)
        self.assertEqual(len(selected), 10)
        case = next((p for p in selected if p.package == 'awaken-session-application'))
        actual = {x.removeprefix('--require-case=') for x in case.arguments if x.startswith('--require-case=')}
        self.assertEqual(actual, EXPECTED_APPLICATION_CASES)
        self.assertEqual(len(actual), 12)

    def test_each_original_outcome_is_independent_and_cannot_be_deleted(self):
        for name in EXPECTED_APPLICATION_CASES:
            with self.subTest(case=name):
                changed = self.source.replace('    --require-case=' + name + ' \\\n', '')
                self.assertNotEqual(changed, self.source)
                with self.assertRaisesRegex(ValueError, 'independent Session application outcomes'):
                    self.check(changed)

    def test_same_package_older_phase_cannot_hide_new_required_outcomes(self):
        changed = self.source
        for name in ORIGINAL_MISSING_FLOORS:
            changed = changed.replace('    --require-case=' + name + ' \\\n', '')
        with self.assertRaisesRegex(ValueError, 'independent Session application outcomes'):
            self.check(changed)

    def test_case_names_on_another_package_do_not_satisfy_application_policy(self):
        changed = self.source
        moved = []
        for name in ORIGINAL_MISSING_FLOORS:
            line = '    --require-case=' + name + ' \\\n'
            self.assertEqual(changed.count(line), 1)
            changed = changed.replace(line, '')
            moved.append(line)
        marker = '  pg_required_test --locked -p awaken-protocol-managed --lib postgres_two_connection_late_hitl_frontier_preserves_old_cursors \\\n'
        self.assertEqual(changed.count(marker), 1)
        changed = changed.replace(marker, marker + ''.join(moved))
        with self.assertRaisesRegex(ValueError, 'independent Session application outcomes'):
            self.check(changed)

    def test_unrelated_new_required_name_does_not_replace_the_original(self):
        name = next(iter(EXPECTED_APPLICATION_CASES))
        changed = self.source.replace('--require-case=' + name, '--require-case=some_other_test', 1)
        with self.assertRaisesRegex(ValueError, 'independent Session application outcomes'):
            self.check(changed)

    def test_original_core_and_storage_domain_policies_are_retained(self):
        self.assertEqual(len(policy.postgres_phases('core', gate.ROOT)), 18)
        self.assertEqual(len(policy.postgres_phases('storage', gate.ROOT)), 8)
        self.assertEqual(len(policy.postgres_phases('storage-observations', gate.ROOT)), 2)


class SessionExtractionFloorTests(unittest.TestCase):
    # Independent test literals: changes to the policy cannot redefine the oracle.
    expected = frozenset({
        'extraction::tests::postgres_extraction_execution_selection_and_first_claim_share_one_revision_fence',
        'extraction::tests::postgres_integer_conversion_rejects_clamping_and_lease_erasure',
    })
    prefix = '  pg_required_test --locked -p awaken-session-store --features test-support --lib postgres_ '

    def setUp(self):
        self.source = (gate.ROOT / 'scripts/ci/pg_tests.sh').read_text(encoding='utf-8')

    def check(self, text):
        with tempfile.TemporaryDirectory(prefix='pg-extraction-policy-') as directory:
            root = Path(directory)
            target = root / 'scripts/ci/pg_tests.sh'
            target.parent.mkdir(parents=True)
            target.write_text(text, encoding='utf-8')
            return policy.postgres_phases('storage', root)

    def statement(self):
        expression = r'(?ms)^  pg_required_test --locked -p awaken-session-store --features test-support --lib postgres_ .*?record_failure "\$\?"\n'
        found = list(re.finditer(expression, self.source))
        self.assertEqual(len(found), 1)
        return found[0].group(0)

    def test_storage_keeps_race_and_pure_conversion_as_distinct_required_outcomes(self):
        self.assertEqual(policy.STORAGE_SESSION_EXTRACTION_CASE_FLOORS, self.expected)
        phases = self.check(self.source)
        self.assertEqual(len(phases), 8)
        phase = next(p for p in phases if p.selector == 'storage_evidence' and p.package == 'awaken-session-store')
        floors = {a.removeprefix('--require-case=') for a in phase.arguments if a.startswith('--require-case=')}
        self.assertTrue(self.expected <= floors)

    def test_each_extraction_outcome_cannot_be_deleted(self):
        for name in self.expected:
            line = '    --require-case=' + name + ' \\\n'
            self.assertEqual(self.source.count(line), 1)
            with self.subTest(case=name), self.assertRaisesRegex(ValueError, 'independent Session extraction outcomes'):
                self.check(self.source.replace(line, '', 1))

    def test_other_package_cannot_supply_the_extraction_outcome(self):
        name = 'extraction::tests::postgres_extraction_execution_selection_and_first_claim_share_one_revision_fence'
        line = '    --require-case=' + name + ' \\\n'
        text = self.source.replace(line, '', 1)
        marker = '    --require-case=postgres_exact_physical_execution_observations_use_the_original_store_clock \\\n'
        self.assertEqual(text.count(marker), 1)
        with self.assertRaisesRegex(ValueError, 'independent Session extraction outcomes'):
            self.check(text.replace(marker, marker + line, 1))

    def test_target_features_and_filter_must_reach_the_required_tests(self):
        statement = self.statement()
        self.assertEqual(statement.count(self.prefix), 1)
        for replacement in (self.prefix.replace('--lib', '--test other_target'),
                            self.prefix.replace('test-support', 'other'),
                            self.prefix.replace('postgres_', 'unrelated_')):
            with self.subTest(replacement=replacement), self.assertRaisesRegex(ValueError, 'independent Session extraction outcomes'):
                self.check(self.source.replace(statement, statement.replace(self.prefix, replacement, 1), 1))

    def test_runtime_aliases_cannot_skip_ignore_or_exact_filter_the_race(self):
        statement = self.statement()
        marker = '-- --test-threads=1'
        self.assertEqual(statement.count(marker), 1)
        for flag in ('--skip unrelated', '--skip=postgres_', '--ignored', '--ignored=true',
                     '--include-ignored', '--include-ignored=true', '--exact'):
            changed = statement.replace(marker, '-- ' + flag + ' --test-threads=1', 1)
            with self.subTest(flag=flag), self.assertRaisesRegex(ValueError, 'independent Session extraction outcomes'):
                self.check(self.source.replace(statement, changed, 1))

    def test_same_package_other_scope_cannot_supply_the_storage_outcome(self):
        name = 'extraction::tests::postgres_extraction_execution_selection_and_first_claim_share_one_revision_fence'
        line = '    --require-case=' + name + ' \\\n'
        marker = '  pg_required_test -p awaken-session-store --features test-support --test session_repo_conformance \\\n'
        self.assertEqual(self.source.count(line), 1)
        self.assertEqual(self.source.count(marker), 1)
        text = self.source.replace(line, '', 1).replace(marker, marker + line, 1)
        with self.assertRaisesRegex(ValueError, 'independent Session extraction outcomes'):
            self.check(text)

if __name__=='__main__':unittest.main(verbosity=2)
