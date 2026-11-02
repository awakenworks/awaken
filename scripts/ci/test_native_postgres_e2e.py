#!/usr/bin/env python3
"""Native served-PG admission, evidence and failure-cause decision tables."""
from __future__ import annotations

import argparse
import contextlib
import copy
import io
import json
from pathlib import Path
import struct
import tempfile
import unittest
from unittest import mock

import check_native_postgres_e2e as gate
import _required_build_context
import rust_test_plan

INPUT_GLOBS=("scripts/ci/**","e2e/**",".github/workflows/windows-native-services.yml")


class ServedPostgresTests(unittest.TestCase):
    def fixture_law_sources(self,root):
        directory=root/"e2e";directory.mkdir()
        paths=[]
        for index in range(2):
            path=directory/f"law_{index}_fixture.test.mjs"
            path.write_text("\n".join(f"test({json.dumps(name)}, () => {{}});" for name in gate.WORKER_FIXTURE_CASES[index::2]),encoding="utf-8")
            paths.append(path)
        return paths

    def test_fixture_discovery_tracks_added_and_relocated_law_owners(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);paths=self.fixture_law_sources(root)
            expected=lambda:tuple(sorted(path.relative_to(root).as_posix() for path in paths))
            (root/"e2e/unrelated_fixture.test.mjs").write_text("test('unrelated', () => {});",encoding="utf-8")
            self.assertEqual(gate.discover_fixture_tests(root),expected())
            moved=root/"e2e/renamed_fixture.test.mjs";paths[0].rename(moved);paths[0]=moved
            self.assertEqual(gate.discover_fixture_tests(root),expected())
            extra=root/"e2e/new_fixture.test.mjs"
            extra.write_text(f"test({json.dumps(gate.WORKER_FIXTURE_CASES[0])}, () => {{}});",encoding="utf-8")
            paths.append(extra)
            self.assertEqual(gate.discover_fixture_tests(root),expected())

    def test_fixture_discovery_rejects_each_missing_required_law(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);paths=self.fixture_law_sources(root)
            originals={path:path.read_text(encoding="utf-8") for path in paths}
            for name in gate.WORKER_FIXTURE_CASES:
                with self.subTest(name=name):
                    for path,source in originals.items():path.write_text(source.replace(name,"missing law"),encoding="utf-8")
                    with self.assertRaisesRegex(ValueError,"no discovered source"):gate.discover_fixture_tests(root)
            for path,source in originals.items():path.write_text(source,encoding="utf-8")
            self.assertEqual(len(gate.discover_fixture_tests(root)),len(paths))

    def test_fixture_discovery_refuses_empty_scope_before_node_autodiscovery(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/"e2e").mkdir()
            with self.assertRaisesRegex(ValueError,"no discovered source"):gate.discover_fixture_tests(root)

    def test_required_case_rejects_missing_duplicate_skipped_and_nonzero_outcomes(self):
        marker=gate.CASES[0][2]
        gate.validate_case("diagnostic\n"+marker+" actual assertions\n",0,marker)
        for text,code in [("",0),("prefix "+marker,0),(marker+"\n"+marker,0),
                          ("SKIP: unavailable\n"+marker,0),("[skip] unavailable\n"+marker,0),
                          ("skipped unavailable\n"+marker,0),(marker,17)]:
            with self.subTest(text=text,code=code),self.assertRaises(ValueError):
                gate.validate_case(text,code,marker)

    @staticmethod
    def codec_log(names=None):
        names=list(gate.WORKER_FIXTURE_CASES if names is None else names)
        return ('TAP version 13\n'+''.join(f'ok {index} - {name}\n' for index,name in enumerate(names,1))
            +f'1..{len(names)}\n# tests {len(names)}\n# pass {len(names)}\n# fail 0\n# cancelled 0\n# skipped 0\n# todo 0\n')

    def test_worker_fixture_codecs_require_actual_unskipped_named_outcomes(self):
        text=self.codec_log();count=len(gate.WORKER_FIXTURE_CASES)
        self.assertEqual(gate.validate_worker_fixture_probe(text,0),list(gate.WORKER_FIXTURE_CASES))
        self.assertEqual(len(gate.validate_worker_fixture_probe(self.codec_log([*gate.WORKER_FIXTURE_CASES,'additional fixture law']),0)),len(gate.WORKER_FIXTURE_CASES)+1)
        for changed,code in ((text,23),('',0),(self.codec_log(gate.WORKER_FIXTURE_CASES[:-1]),0),
                (self.codec_log([*gate.WORKER_FIXTURE_CASES,gate.WORKER_FIXTURE_CASES[0]]),0),
                (text.replace('# skipped 0','# skipped 1'),0),(text.replace(f'# pass {count}','# pass 0'),0),
                (text+f'# tests {count}\n',0),(text.replace(f'1..{count}',f'1..{count-1}'),0)):
            with self.subTest(changed=changed,code=code),self.assertRaises(ValueError):
                gate.validate_worker_fixture_probe(changed,code)

    def test_sql_observation_requires_one_actual_unskipped_law_and_exact_totals(self):
        text='ok 1 - '+gate.SQL_CASE+'\n# tests 1\n# pass 1\n# fail 0\n# cancelled 0\n# skipped 0\n'
        gate.validate_sql_probe(text,0)
        for changed,code in ((text,17),(text.replace('ok 1 - ','ok 1 - foreign '),0),
                (text.replace('# skipped 0','# skipped 1'),0),(text.replace('# pass 1','# pass 0'),0),
                (text+'# tests 1\n',0),(text+'SKIP: unavailable\n',0)):
            with self.subTest(text=changed,code=code),self.assertRaises(ValueError):gate.validate_sql_probe(changed,code)

    def test_bundle_requires_source_every_role_distinct_owned_paths_and_unchanged_native_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binaries=root/"binaries";binaries.mkdir()
            rows={}
            for name in ("production","scenario","hand"):
                executable=binaries/(name+".exe")
                content=bytearray(70);content[:2]=b"MZ";struct.pack_into("<I",content,60,64)
                content[64:68]=b"PE\0\0";struct.pack_into("<H",content,68,0x8664)
                executable.write_bytes(content+name.encode())
                rows[name]=dict(path=str(executable),sha256=gate.sha256(executable))
            original=dict(status="built",source_digest="a"*64,binaries=rows)
            record=root/"build.json"
            def validate(value):
                record.write_text(json.dumps(value));return gate.validate_bundle(record,"a"*64)
            self.assertEqual(validate(original),rows)
            for alteration in ("source","status","missing-role","duplicate-path","outside","changed-hash"):
                value=copy.deepcopy(original)
                if alteration=="source":value["source_digest"]="b"*64
                elif alteration=="status":value["status"]="passed"
                elif alteration=="missing-role":del value["binaries"]["hand"]
                elif alteration=="duplicate-path":value["binaries"]["scenario"]=value["binaries"]["production"].copy()
                elif alteration=="outside":value["binaries"]["hand"]["path"]=str(root/"foreign.exe")
                else:value["binaries"]["hand"]["sha256"]="0"*64
                with self.subTest(alteration=alteration),self.assertRaises(ValueError):validate(value)
            (binaries/"hand.exe").write_bytes(b"not a PE")
            with self.assertRaises(ValueError):validate(original)

    def test_case_environment_excludes_external_services_and_injects_only_owned_endpoints(self):
        inherited=dict(PATH="tools",SystemRoot="system",AWAKEN_NATIVE_BASH="native-bash",
            AWAKEN_E2E_POSTGRES_CONTAINER="foreign",AWAKEN_E2E_AWAKEN_BIN="foreign",
            SESSION_DEPLOYMENT_DATABASE_URL="foreign",PGHOST="foreign",AWS_ENDPOINT_URL="foreign",
            MINIO_ROOT_USER="foreign",MC_HOST_external="foreign")
        binaries={role:dict(path=role+".exe") for role in ("production","scenario","hand")}
        value=gate.case_environment(inherited,"owned-db",Path("owned-temp"),binaries,Path("pg/psql.exe"))
        self.assertEqual(value["SESSION_DEPLOYMENT_DATABASE_URL"],"owned-db")
        self.assertEqual(value["AWAKEN_E2E_AWAKEN_BIN"],"production.exe")
        self.assertEqual(value["AWAKEN_E2E_SCENARIO_HOST_BIN"],"scenario.exe")
        self.assertEqual(value["AWAKEN_NATIVE_BASH"],"native-bash")
        self.assertEqual(value["SystemRoot"],"system")
        self.assertNotEqual(value["E2E_PORT"],value["E2E_WORKER_PORT"])
        for name in ("AWAKEN_E2E_POSTGRES_CONTAINER","PGHOST","AWS_ENDPOINT_URL","MINIO_ROOT_USER","MC_HOST_external"):
            self.assertNotIn(name,value)

    def fixture(self,root):
        fixture=root/"fixture";fixture.mkdir()
        args=argparse.Namespace(output=root/"out",temp_root=root,node_bin=root/"node.exe",postgres_bin=root/"pg")
        service=mock.Mock(url="postgres://fixture@127.0.0.1:12345/awaken_ci",binaries={"psql":{"path":"psql.exe"}})
        service.start.return_value={"owner":"owned"};service.close.return_value={"stopped":True,"exit_code":0}
        binaries={role:dict(path=role+".exe") for role in ("production","scenario","hand")}
        stack=contextlib.ExitStack()
        facts={"revision":b"revision","index_tree":b"tree","patch":b"patch"}
        stack.enter_context(mock.patch.object(rust_test_plan,"working_subject",return_value=("a"*64,facts)))
        stack.enter_context(mock.patch.object(_required_build_context,"build_context",return_value={}))
        stack.enter_context(mock.patch.object(gate.tempfile,"mkdtemp",return_value=str(fixture)))
        stack.enter_context(mock.patch.object(gate,"windows_executable",return_value={"path":"node.exe"}))
        constructor=stack.enter_context(mock.patch.object(gate,"PostgresService",return_value=service))
        stack.enter_context(mock.patch.object(gate,"validate_bundle",return_value=binaries))
        return stack,args,service,constructor

    @staticmethod
    def child(command,**kwargs):
        name=Path(kwargs["stdout"].name).stem
        text="host: x86_64-pc-windows-msvc\n" if name=="rustc-identity" else ""
        text+=next((marker+" actual assertions\n" for case,_,marker in gate.CASES if case==name),"")
        if name=="psql-observation":text='ok 1 - '+gate.SQL_CASE+'\n# tests 1\n# pass 1\n# fail 0\n# cancelled 0\n# skipped 0\n'
        if name=="worker-fixture-codec":text=ServedPostgresTests.codec_log()
        kwargs["stdout"].write(text.encode())
        return mock.Mock(returncode=0)

    def test_success_requires_every_actual_case_source_recheck_and_clean_owned_shutdown(self):
        with tempfile.TemporaryDirectory() as directory:
            stack,args,service,_=self.fixture(Path(directory))
            events=[]
            def started():events.append("start-postgres");return {"owner":"owned"}
            def child(command,**kwargs):events.append(Path(kwargs["stdout"].name).stem);return self.child(command,**kwargs)
            service.start.side_effect=started
            with stack,mock.patch.object(gate.subprocess,"run",side_effect=child),contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gate.worker(args),0)
            value=json.loads((args.output/"result.json").read_text())
            self.assertEqual([row["case"] for row in value["cases"]],[row[0] for row in gate.CASES])
            self.assertIs(value["source_unchanged"],True)
            self.assertIs(value["full_gate"],False)
            self.assertLess(events.index("worker-fixture-codec"),events.index("build-execution-bundles"))
            self.assertLess(events.index("build-execution-bundles"),events.index("start-postgres"))
            self.assertLess(events.index("start-postgres"),events.index("durable-commit"))
            service.start.assert_called_once();self.assertEqual(service.verify.call_count, len(gate.CASES)+1);service.close.assert_called_once()

    def test_each_case_gets_a_distinct_owned_database_before_execution(self):
        names=[gate.case_database_name(case) for case,_,_ in gate.CASES]
        self.assertEqual(len(names),len(set(names)))
        for case in ("", "unknown", "../foreign", 'foreign;DROP DATABASE awaken_ci'):
            with self.subTest(case=case),self.assertRaises(ValueError):gate.case_database_name(case)
        with tempfile.TemporaryDirectory() as directory:
            stack,args,service,_=self.fixture(Path(directory));created=[];observed=[]
            service.sql.side_effect=lambda sql:created.append(sql)
            def child(command,**kwargs):
                stage=Path(kwargs["stdout"].name).stem
                if stage in {row[0] for row in gate.CASES}:
                    database=gate.case_database_name(stage)
                    self.assertEqual(created[-1],"CREATE DATABASE "+database)
                    self.assertEqual(kwargs["env"]["SESSION_DEPLOYMENT_DATABASE_URL"],
                                     service.url.rsplit("/",1)[0]+"/"+database)
                    observed.append(database)
                return self.child(command,**kwargs)
            with stack,mock.patch.object(gate.subprocess,"run",side_effect=child),contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gate.worker(args),0)
            self.assertEqual(created,["CREATE DATABASE "+name for name in names])
            self.assertEqual(observed,names)
            receipt=json.loads((args.output/"result.json").read_text())
            self.assertEqual([row["database"] for row in receipt["cases"]],names)

    def test_database_creation_failure_starts_no_case_and_still_stops_owned_postgres(self):
        with tempfile.TemporaryDirectory() as directory:
            stack,args,service,_=self.fixture(Path(directory));stages=[]
            service.sql.side_effect=RuntimeError("owned database creation refused")
            def child(command,**kwargs):
                stages.append(Path(kwargs["stdout"].name).stem)
                return self.child(command,**kwargs)
            with stack,mock.patch.object(gate.subprocess,"run",side_effect=child),contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gate.worker(args),1)
            self.assertFalse({row[0] for row in gate.CASES}.intersection(stages))
            service.close.assert_called_once()
            receipt=json.loads((args.output/"result.json").read_text())
            self.assertEqual(receipt["cases"],[])
            self.assertEqual(receipt["status"],"failed")

    def test_fixture_codec_refusal_precedes_binary_build_and_database_start(self):
        with tempfile.TemporaryDirectory() as directory:
            stack,args,service,_=self.fixture(Path(directory));events=[]
            def child(command,**kwargs):
                name=Path(kwargs["stdout"].name).stem;events.append(name)
                if name=="worker-fixture-codec":return mock.Mock(returncode=23)
                return self.child(command,**kwargs)
            with stack,mock.patch.object(gate.subprocess,"run",side_effect=child),contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gate.worker(args),23)
            self.assertIn("worker-fixture-codec",events)
            self.assertNotIn("build-execution-bundles",events)
            service.start.assert_not_called();service.close.assert_called_once()
            receipt=json.loads((args.output/"result.json").read_text())
            self.assertEqual(receipt["stages"][-1]["exit_code"],23)
            self.assertEqual(receipt["cases"],[])

    def test_independent_receipt_audit_rejects_omissions_tampering_and_wrong_ownership(self):
        with tempfile.TemporaryDirectory() as directory:
            stack,args,service,_=self.fixture(Path(directory))
            with stack,mock.patch.object(gate.subprocess,"run",side_effect=self.child),contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gate.worker(args),0)
            record=args.output/"result.json";original=json.loads(record.read_text())
            owner=Path(original["fixture_root"])/"owner.json";ownership=json.loads(owner.read_text())
            original["fixture"]["owner"]=ownership["owner"]
            record.write_text(json.dumps(original))
            with mock.patch.object(gate,"validate_bundle",return_value=original["binaries"]),mock.patch.object(rust_test_plan,"working_subject",return_value=("a"*64,{})):
                self.assertEqual(gate.reconcile_execution(args.output)["status"],"passed")
                for fault in ("missing-case","duplicate-case","missing-preparation","unclean-stop","wrong-case-source","wrong-case-hash","wrong-owner","stale-source","wrong-patch","missing-fixture-probe","wrong-fixture-hash","missing-case-database","shared-case-database","wrong-case-database"):
                    value=copy.deepcopy(original)
                    if fault=="missing-case":value["cases"].pop()
                    elif fault=="duplicate-case":value["cases"][-1]=value["cases"][0].copy()
                    elif fault=="missing-preparation":value["stages"].pop(0)
                    elif fault=="unclean-stop":value["cleanup"]["exit_code"]=8
                    elif fault=="wrong-case-source":value["cases"][0]["source"]="e2e/foreign.mjs"
                    elif fault=="wrong-case-hash":value["cases"][0]["source_sha256"]="0"*64
                    elif fault=="wrong-owner":value["fixture"]["owner"]="foreign"
                    elif fault=="stale-source":value["source"]["digest"]="b"*64
                    elif fault=="missing-case-database":del value["cases"][0]["database"]
                    elif fault=="shared-case-database":value["cases"][1]["database"]=value["cases"][0]["database"]
                    elif fault=="wrong-case-database":value["cases"][0]["database"]="foreign"
                    elif fault=="missing-fixture-probe":del value["worker_fixture_probe"]
                    elif fault=="wrong-fixture-hash":value["worker_fixture_probe"]["sources"][gate.FIXTURE_TESTS[1]]="0"*64
                    else:value["source"]["patch_sha256"]="0"*64
                    record.write_text(json.dumps(value))
                    with self.subTest(fault=fault),self.assertRaises(ValueError):gate.reconcile_execution(args.output)
                record.write_text(json.dumps(original))
                log=args.output/"durable-commit.log";before=log.read_bytes();log.write_bytes(before+b"tampered")
                with self.assertRaises(ValueError):gate.reconcile_execution(args.output)
                log.write_bytes(before)
                self.assertEqual(gate.reconcile_execution(args.output)["status"],"passed")

    def test_independent_audit_checks_actual_temporary_state_instead_of_trusting_flags(self):
        with tempfile.TemporaryDirectory() as directory:
            stack,args,service,_=self.fixture(Path(directory))
            with stack,mock.patch.object(gate.subprocess,"run",side_effect=self.child),contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gate.worker(args),0)
            record=args.output/"result.json";value=json.loads(record.read_text())
            root=Path(value["fixture_root"])
            value["fixture"]["owner"]=json.loads((root/"owner.json").read_text())["owner"]
            record.write_text(json.dumps(value))
            with mock.patch.object(gate,"validate_bundle",return_value=value["binaries"]),mock.patch.object(rust_test_plan,"working_subject",return_value=("a"*64,{})):
                for name in ("observation",*[row[0] for row in gate.CASES]):
                    temporary=root/"cases"/name;sentinel=temporary/"retained"
                    sentinel.write_text("retained")
                    with self.subTest(case=name),self.assertRaises(ValueError):gate.reconcile_execution(args.output)
                    sentinel.unlink();temporary.rmdir()
                    with self.assertRaises(ValueError):gate.reconcile_execution(args.output)
                    temporary.mkdir()
                self.assertEqual(gate.reconcile_execution(args.output)["status"],"passed")

    def test_atomic_receipt_failure_preserves_previous_record(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);gate.save_record(root,{"status":"running"});before=(root/"result.json").read_bytes()
            with mock.patch.object(Path,"replace",side_effect=OSError("replacement unavailable")):
                with self.assertRaises(OSError):gate.save_record(root,{"status":"passed"})
            self.assertEqual((root/"result.json").read_bytes(),before)
            with self.assertRaises(TypeError):gate.save_record(root,{"status":object()})
            self.assertEqual((root/"result.json").read_bytes(),before)
            gate.save_record(root,{"status":"failed"})
            self.assertFalse((root/"result.pending").exists())

    def test_admission_failure_precedes_service_construction_and_build(self):
        with tempfile.TemporaryDirectory() as directory:
            stack,args,service,constructor=self.fixture(Path(directory))
            with stack,mock.patch.object(gate.subprocess,"run",return_value=mock.Mock(returncode=75)) as child,contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gate.worker(args),75)
            constructor.assert_not_called();service.start.assert_not_called()
            self.assertEqual(child.call_count,1)
            self.assertIn("disk",child.call_args.args[0])
            self.assertEqual(child.call_args.args[0][-2:],["--stage","rust-full"])

    def test_child_cause_survives_hash_record_and_shutdown_faults(self):
        for phase in ("hash","stage","terminal"):
            for child_status in (0,17):
                for cleanup_fails in (False,True):
                    with self.subTest(phase=phase,child=child_status,cleanup=cleanup_fails),tempfile.TemporaryDirectory() as directory:
                        stack,args,service,_=self.fixture(Path(directory));snapshots=[];faults=[]
                        if cleanup_fails:service.close.side_effect=RuntimeError("controlled cleanup failure")
                        def child(command,**kwargs):
                            result=self.child(command,**kwargs)
                            if Path(kwargs["stdout"].name).stem=="durable-commit":result.returncode=child_status
                            return result
                        def fail():faults.append(phase);raise OSError("controlled "+phase+" evidence failure")
                        def save(_output,value):
                            if not faults and ((phase=="stage" and any(row["stage"]=="durable-commit" for row in value["stages"]))
                                    or (phase=="terminal" and value["status"]!="running")):fail()
                            snapshots.append(copy.deepcopy(value))
                        def digest(path):
                            if phase=="hash" and Path(path).name=="durable-commit.log" and not faults:fail()
                            return "0"*64
                        with stack,mock.patch.object(gate.subprocess,"run",side_effect=child),mock.patch.object(gate,"save_record",side_effect=save),mock.patch.object(gate,"sha256",side_effect=digest),contextlib.redirect_stdout(io.StringIO()):
                            self.assertEqual(gate.worker(args),child_status or 1)
                        self.assertEqual(faults,[phase]);service.close.assert_called_once()
                        self.assertEqual(snapshots[-1]["status"],"failed")
                        self.assertEqual(snapshots[-1]["exit_code"],child_status or 1)

    def test_interrupt_and_unclean_shutdown_cannot_create_a_passing_receipt(self):
        for mode in ("interrupt","not-stopped","nonzero-exit","source-changed"):
            with self.subTest(mode=mode),tempfile.TemporaryDirectory() as directory:
                stack,args,service,_=self.fixture(Path(directory))
                if mode=="not-stopped":service.close.return_value={"stopped":False}
                if mode=="nonzero-exit":service.close.return_value={"stopped":True,"exit_code":7}
                def child(command,**kwargs):
                    if mode=="interrupt" and Path(kwargs["stdout"].name).stem=="durable-commit":raise KeyboardInterrupt()
                    return self.child(command,**kwargs)
                with stack,mock.patch.object(gate.subprocess,"run",side_effect=child),contextlib.redirect_stdout(io.StringIO()):
                    if mode=="source-changed":
                        rust_test_plan.working_subject.side_effect=[("a"*64,{"revision":b"r","index_tree":b"t","patch":b""}),("b"*64,{})]
                    status=gate.worker(args)
                self.assertEqual(status,130 if mode=="interrupt" else 1)
                self.assertEqual(json.loads((args.output/"result.json").read_text())["status"],"failed")
                service.close.assert_called_once()

    def test_initial_evidence_failure_precedes_fixture_allocation(self):
        with tempfile.TemporaryDirectory() as directory:
            stack,args,service,constructor=self.fixture(Path(directory))
            with stack,mock.patch.object(gate,"save_record",side_effect=OSError("initial evidence unavailable")),mock.patch.object(gate.subprocess,"run") as child:
                with self.assertRaises(OSError):gate.worker(args)
                gate.tempfile.mkdtemp.assert_not_called();constructor.assert_not_called();child.assert_not_called()


if __name__=="__main__":unittest.main(verbosity=2)
