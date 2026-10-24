#!/usr/bin/env python3
"""Selection, admission and execution-receipt fault tables for native storage."""
from __future__ import annotations

import contextlib
import copy
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
PACKAGES = ("awaken-sql-storage", "awaken-session-store", "awaken-config-store",
            "awaken-store-postgres", "awaken-env-store", "awaken-file-store",
            "awaken-store-schema", "awaken-store-sqlite")


class StorageSelectionTests(unittest.TestCase):
    def selection(self, status, name="storage_evidence"):
        source=(gate.ROOT/"scripts/ci/pg_tests.sh").read_text(encoding="utf-8")
        def function(name):
            match=re.search(r"(?ms)^"+name+r"\(\) \{\n.*?^\}",source)
            self.assertIsNotNone(match,name)
            return match.group(0)
        with tempfile.TemporaryDirectory(prefix="storage selector ") as directory:
            root=Path(directory);calls=root/"calls";script=root/"selection.sh"
            # Only the real selector and failure composer are evaluated. The
            # stub records argv; this unit check never starts Cargo or a DB.
            script.write_text('''#!/usr/bin/env bash
set -uo pipefail
first_failure=0
pg_required_test() {
  printf '%s\\0' "$@" >>"$CALLS"
  printf '\\n' >>"$CALLS"
  return "$RUNNER_STATUS"
}
'''+function("record_failure")+'\n'+function(name)+
                '\n'+name+'\nexit "$first_failure"\n',encoding="utf-8",newline="\n")
            child=subprocess.run([sys.executable,str(gate.ROOT/"scripts/ci/_native_shell.py"),script.as_posix()],
                env=dict(os.environ,CALLS=calls.as_posix(),RUNNER_STATUS=str(status)),
                capture_output=True,timeout=30)
            rows=[row.decode().split("\0")[:-1] for row in calls.read_bytes().split(b"\n") if row]
            return child.returncode,rows

    def test_selector_keeps_all_eight_domains_features_ignored_cases_and_named_floors(self):
        code,rows=self.selection(0);self.assertEqual(code,0)
        self.assertEqual([row[row.index("-p")+1] for row in rows],list(PACKAGES))
        for row in rows:
            self.assertIn("--locked",row);self.assertIn("--test-threads=1",row)
            self.assertTrue(any(value.startswith("--require-case=") for value in row[:row.index("--")]))
        runtime=rows[0]
        self.assertIn("--ignored",runtime)
        self.assertIn("checkout_health_policy_performance_observation",runtime)
        self.assertIn("--require-case=postgres_tests::cancelled_begin_cannot_poison_the_next_recovery_transaction",runtime)
        self.assertEqual(rows[1][rows[1].index("--features")+1],"test-support")
        file=rows[5];self.assertEqual(file[file.index("--features")+1],"postgres,sqlite,object-store")
        self.assertEqual(file[file.index("--skip")+1],"minio_")
        self.assertNotIn("--include-ignored",rows[3])
        self.assertNotIn("--ignored",rows[3])
        self.assertEqual([rows[3][i+1] for i,x in enumerate(rows[3]) if x=="--skip"],['chunk_boundary_commit_throughput_observation', 'hot_thread_history_open_reports_its_scoped_cost', 'independent_thread_commit_throughput_observation', 'million_historical_facts_do_not_gate_startup_or_unrelated_thread_open', 'multi_fact_commit_throughput_observation'])
        sqlite=rows[7]
        self.assertEqual(sqlite[sqlite.index("--test")+1],"failure_atomicity")
        for case in ("corrupt_uncommitted_wal_never_projects_a_partial_commit","sqlite_full_rolls_back_the_complete_domain_commit",
                     "sqlite_read_only_rolls_back_the_complete_domain_commit","uncommitted_transaction_is_rolled_back_after_process_kill"):
            self.assertIn("--require-case="+case,sqlite)

    def test_ordinary_failure_and_timeout_preserve_status_while_collecting_remaining_domains(self):
        for status in (23,124):
            with self.subTest(status=status):
                code,rows=self.selection(status);self.assertEqual(code,status);self.assertEqual(len(rows),8)

    def test_resource_refusal_stops_before_a_second_domain(self):
        code,rows=self.selection(75);self.assertEqual(code,75);self.assertEqual(len(rows),1)

    def test_explicit_observations_require_both_domains_all_six_names_and_ignored_execution(self):
        code,rows=self.selection(0,"storage_observation_evidence");self.assertEqual(code,0)
        self.assertEqual([row[row.index("-p")+1] for row in rows],["awaken-sql-storage","awaken-store-postgres"])
        self.assertEqual([sum(x.startswith("--require-case=") for x in row) for row in rows],[1,5])
        for row in rows:
            self.assertIn("--ignored",row);self.assertNotIn("--nocapture",row);self.assertIn("--locked",row)
            self.assertNotIn("--skip",row)
        self.assertIn("--exact",rows[0])
        self.assertEqual([x.removeprefix("--require-case=") for x in rows[1] if x.startswith("--require-case=")],['chunk_boundary_commit_throughput_observation', 'hot_thread_history_open_reports_its_scoped_cost', 'independent_thread_commit_throughput_observation', 'million_historical_facts_do_not_gate_startup_or_unrelated_thread_open', 'multi_fact_commit_throughput_observation'])
        self.assertEqual(postgres_plan("storage-observations"),[("postgres-storage-observations","storage-observations.log",["scripts/ci/pg_tests.sh","--storage-observations"])])

    def test_observations_keep_failure_timeout_and_stop_on_resource_refusal(self):
        for status,count in ((23,2),(124,2),(75,1)):
            with self.subTest(status=status):
                code,rows=self.selection(status,"storage_observation_evidence")
                self.assertEqual(code,status);self.assertEqual(len(rows),count)

    def test_native_plan_has_one_storage_entry_and_retains_the_four_core_entries(self):
        self.assertEqual(postgres_plan("storage"),[("postgres-storage","storage.log",["scripts/ci/pg_tests.sh","--storage"])])
        self.assertEqual([row[2][-1] for row in postgres_plan("core")],["--backend-parity","--work-store","--http","--critical"])
        with self.assertRaises(ValueError):postgres_plan("unknown")

    def test_other_services_refuse_postgres_scope_before_any_launch(self):
        for service in ("nats","s3"):
            with self.subTest(service=service),contextlib.redirect_stderr(io.StringIO()),mock.patch.object(gate,"worker") as worker:
                with self.assertRaises(SystemExit) as error:
                    gate.main(["--service",service,"--postgres-scope","storage","--output","not-created"])
                self.assertEqual(error.exception.code,2);worker.assert_not_called()

    def test_native_workflow_requires_the_storage_entry(self):
        text=(gate.ROOT/".github/workflows/windows-native-services.yml").read_text(encoding="utf-8")
        self.assertIn("--service postgres --postgres-scope storage --postgres-bin",text)


class StorageReceiptTests(unittest.TestCase):
    def receipts(self,base):
        paths=[]
        for index,package in enumerate(PACKAGES):
            path=base/"test-timings"/str(index);path.mkdir(parents=True);case="storage_law_"+str(index)
            doc={"status":"passed","exit_code":0,"source_unchanged":True,"source_digest":"source",
                 "selected_cases":[case],"executed_cases":[[case,"ok"]],"required_cases":[case],
                 "arguments":["-p",package],"postgres_fixture_required":True,"postgres_fixture_configured":True}
            (path/"results.json").write_text(json.dumps(doc),encoding="utf-8")
            (path/"list.log").write_text(case+": test\n",encoding="utf-8")
            (path/"results.log").write_text("test "+case+" ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n",encoding="utf-8")
            paths.append(path)
        return paths

    def test_all_domains_require_exact_source_live_fixture_and_actual_executions(self):
        import check_rust_test_execution as q
        with tempfile.TemporaryDirectory() as directory,mock.patch.object(q,"verify_preparation"):
            base=Path(directory);paths=self.receipts(base)
            def references(rows):return "".join("execution evidence: "+str(path)+"\n" for path in rows)
            log=references(paths)
            with mock.patch.dict(os.environ,{"CARGO_TARGET_DIR":str(base)}):
                self.assertEqual(len(gate.reconcile_postgres(log,"source","storage")),8)
                for rows in (paths[:-1],[*paths[:-1],paths[-2]]):
                    with self.assertRaises(ValueError):gate.reconcile_postgres(references(rows),"source","storage")
                with self.assertRaises(ValueError):gate.reconcile_postgres(log,"wrong-source","storage")
                path=paths[0]/"results.json";original=json.loads(path.read_text())
                changes=[{"arguments":["-p","awaken-memory-store"]}]
                for key in ("postgres_fixture_required","postgres_fixture_configured"):
                    changes.extend({key:value} for value in (False,"1",None))
                for changed in changes:
                    value=copy.deepcopy(original);value.update(changed);path.write_text(json.dumps(value),encoding="utf-8")
                    with self.subTest(change=changed),self.assertRaises(ValueError):gate.reconcile_postgres(log,"source","storage")
                path.write_text(json.dumps(original),encoding="utf-8")
                path=paths[0]/"results.log";original_output=path.read_text()
                for changed in (original_output.replace(" ... ok"," ... ignored"),original_output+"SKIPPED PostgreSQL fixture\n"):
                    path.write_text(changed,encoding="utf-8")
                    with self.assertRaises(ValueError):gate.reconcile_postgres(log,"source","storage")


if __name__=="__main__":
    unittest.main(verbosity=2)
