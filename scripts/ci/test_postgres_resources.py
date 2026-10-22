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
from _native_postgres_scopes import postgres_plan

INPUT_GLOBS = ("scripts/ci/**", ".github/workflows/windows-native-services.yml")

PACKAGES=('awaken-admin-config-api','awaken-resource-store','awaken-resource-persistence',
    'awaken-session-application','awaken-protocol-managed','awaken-sandbox-policy-store',
    'awaken-executable-agent-catalog','awaken-executable-environment-catalog','awaken-environment-image-build')

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

    def test_all_nine_domains_keep_features_named_floor_and_new_admission_laws(self):
        code,rows=self.selection(0);self.assertEqual(code,0)
        self.assertEqual([row[row.index('-p')+1] for row in rows],list(PACKAGES))
        for row in rows:
            self.assertIn('--locked',row);self.assertIn('--test-threads=1',row)
            self.assertTrue(any(x.startswith('--require-case=') for x in row[:row.index('--')]))
        self.assertEqual(rows[0][rows[0].index('--features')+1],'postgres')
        self.assertIn('--all-features',rows[1]);self.assertIn('--exact',rows[-1])
        counts=[sum(x.endswith('_fixture_admission') for x in row if x.startswith('--require-case=')) for row in rows]
        self.assertEqual(counts,[0,4,3,3,1,0,0,1,0])
        # The two filtered groups must include the original domain and its new
        # admission case. An exact original-only filter would erase the latter.
        self.assertNotIn('--exact',rows[3]);self.assertNotIn('--exact',rows[4])

    def test_execution_failure_and_timeout_preserve_first_status_and_other_domains(self):
        for status in (23,124):
            with self.subTest(status=status):
                code,rows=self.selection(status);self.assertEqual(code,status);self.assertEqual(len(rows),9)

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
            base=Path(directory);paths=[]
            for index,package in enumerate(PACKAGES):
                path=base/'test-timings'/str(index);path.mkdir(parents=True);case='resource_law_'+str(index)
                doc=dict(status='passed',exit_code=0,source_unchanged=True,source_digest='source',selected_cases=[case],
                    executed_cases=[[case,'ok']],required_cases=[case],arguments=['-p',package],
                    postgres_fixture_required=True,postgres_fixture_configured=True)
                (path/'results.json').write_text(json.dumps(doc),encoding='utf-8')
                (path/'list.log').write_text(case+': test\n',encoding='utf-8')
                (path/'results.log').write_text('test '+case+' ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n',encoding='utf-8')
                paths.append(path)
            def log(paths):return ''.join('execution evidence: '+str(path)+'\n' for path in paths)
            with mock.patch.dict(os.environ,{'CARGO_TARGET_DIR':str(base)}):
                self.assertEqual(len(gate.reconcile_postgres(log(paths),'source','resources')),9)
                for rows in (paths[:-1],[*paths[:-1],paths[-2]]):
                    with self.assertRaises(ValueError):gate.reconcile_postgres(log(rows),'source','resources')
                with self.assertRaises(ValueError):gate.reconcile_postgres(log(paths),'wrong-source','resources')
                path=paths[0]/'results.json';original=json.loads(path.read_text())
                for change in ({'arguments':['-p','awaken-memory-store']},{'postgres_fixture_required':False},{'postgres_fixture_configured':False}):
                    path.write_text(json.dumps(dict(original,**change)),encoding='utf-8')
                    with self.subTest(change=change),self.assertRaises(ValueError):gate.reconcile_postgres(log(paths),'source','resources')
                path.write_text(json.dumps(original),encoding='utf-8')
                path=paths[0]/'results.log';original=path.read_text()
                for output in (original.replace(' ... ok',' ... ignored'),original+'SKIPPED PostgreSQL fixture\n'):
                    path.write_text(output,encoding='utf-8')
                    with self.assertRaises(ValueError):gate.reconcile_postgres(log(paths),'source','resources')

if __name__=='__main__':unittest.main(verbosity=2)
