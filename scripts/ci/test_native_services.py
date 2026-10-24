#!/usr/bin/env python3
"""Decision tables for owned native service processes and truthful receipts."""
from __future__ import annotations

import argparse
import copy
import json
import os
import secrets
from pathlib import Path
import struct
import subprocess
import tempfile
import unittest
from unittest import mock

import check_native_services as gate
import native_services as services
import _native_postgres as pg
import _native_s3 as s3

INPUT_GLOBS = ("scripts/ci/**", ".github/workflows/windows-native-services.yml")


class NativeServiceTests(unittest.TestCase):
    def test_native_binary_requires_an_intact_amd64_pe_before_launch(self):
        with tempfile.TemporaryDirectory() as directory:
            executable=Path(directory)/"service.exe"
            valid=bytearray(90)
            valid[:2]=b"MZ"
            valid[60:64]=struct.pack("<I",70)
            valid[70:76]=b"PE\0\0"+struct.pack("<H",0x8664)
            executable.write_bytes(valid)
            self.assertEqual(services.windows_executable(executable)["abi"],"windows-amd64")
            for changed in (b"not an executable",b"MZ",valid[:73],valid[:74]+b"\x4c\x01"+valid[76:]):
                executable.write_bytes(changed)
                with self.subTest(length=len(changed)),self.assertRaises(ValueError):
                    services.windows_executable(executable)

    def test_readiness_requires_exact_owned_protocol_identity(self):
        # R1 exact INFO/name/port/id -> admitted. Each one-premise mutation
        # independently refuses the endpoint before tests can consume it.
        identity={"server_name":"owned","port":1234,"server_id":"one","version":"2.15.0"}
        frame=lambda value:b"INFO "+json.dumps(value).encode()+b"\r\n"
        self.assertEqual(services.decode_nats_info(frame(identity),"owned",1234),identity)
        for changed in (dict(identity,server_name="foreign"),dict(identity,port=4321),
                        dict(identity,server_id=""),[identity]):
            with self.subTest(identity=changed),self.assertRaises(ValueError):
                services.decode_nats_info(frame(changed),"owned",1234)
        for line in (b"PONG\r\n",b"INFO {}",b"INFO "+b"x"*65537+b"\n"):
            with self.assertRaises(ValueError):services.decode_nats_info(line,"owned",1234)

    def test_cleanup_uses_the_retained_handle_and_preserves_failure(self):
        service=object.__new__(services.NatsService)
        service.process=mock.Mock(pid=987,returncode=1)
        service.process.poll.return_value=None
        service.log=mock.Mock()
        result=service.close()
        self.assertTrue(result["stopped"])
        service.process.terminate.assert_called_once_with()
        service.process.wait.assert_called_once_with(timeout=10)
        service.log.close.assert_called_once_with()
        service.process.wait.side_effect=subprocess.TimeoutExpired("owned-process",10)
        with self.assertRaises(subprocess.TimeoutExpired):service.close()

    def test_unexpected_process_exit_or_changed_endpoint_is_not_readiness(self):
        service=object.__new__(services.NatsService)
        service.process=mock.Mock()
        service.process.poll.return_value=2
        with self.assertRaisesRegex(RuntimeError,"no longer running"):service.verify()
        service.process.poll.return_value=None
        service.port=1234;service.owner="owned";service.identity={"server_id":"one"}
        with mock.patch.object(services,"nats_info",return_value={"server_id":"another"}):
            with self.assertRaisesRegex(RuntimeError,"identity changed"):service.verify()



class NativeInstallTests(unittest.TestCase):
    def test_archive_refuses_path_escape_and_symlinks(self):
        import install_windows_services as installer
        import zipfile
        for name, mode in [("../outside.exe", 0), ("/outside.exe", 0),
                           ("C:/outside.exe", 0), ("dir\\outside.exe", 0),
                           (installer.PINS["nats"]["executable"], 0o120777 << 16)]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                base=Path(directory);archive=base/"archive.zip";target=base/"owned";target.mkdir()
                with zipfile.ZipFile(archive,"w") as bundle:
                    info=zipfile.ZipInfo(name);info.filename=name;info.external_attr=mode;bundle.writestr(info,b"bad")
                with self.assertRaises(ValueError):installer.extract_runtime(archive,target,"nats")
                self.assertFalse(any(target.rglob("*")))

    @unittest.skipUnless(os.name == "nt", "native Windows installer requires Windows")
    def test_checksum_failure_records_failure_before_any_extraction(self):
        import install_windows_services as installer
        with tempfile.TemporaryDirectory() as directory:
            base=Path(directory);archives=base/"archives";archives.mkdir()
            pin=installer.PINS["nats"]
            (archives/pin["archive"]).write_bytes(b"wrong archive")
            with mock.patch.object(installer,"extract_runtime") as extract:
                with self.assertRaisesRegex(ValueError,"reviewed checksum"):
                    installer.install(base/"output",archives,"nats")
                extract.assert_not_called()
            self.assertEqual(json.loads((base/"output/installation.json").read_text())["status"],"failed")
            with self.assertRaises(FileExistsError):installer.install(base/"output",archives,"nats")

    def test_windows_ci_selects_native_owners_and_preserves_failure_evidence(self):
        workflow=(gate.ROOT/".github/workflows/windows-native-services.yml").read_text(encoding="utf-8")
        self.assertEqual(workflow.count("runs-on:"),1)
        self.assertIn("runs-on: windows-2025",workflow)
        self.assertNotIn("continue-on-error",workflow)
        self.assertNotIn("container:",workflow)
        self.assertIn("if: always()",workflow)
        self.assertIn("python scripts/ci/install_windows_services.py --service nats",workflow)
        self.assertIn("python scripts/ci/check_native_services.py --service nats",workflow)
        self.assertIn("target/windows-services/installation.json",workflow)
        self.assertIn("python scripts/ci/install_windows_services.py --service postgres",workflow)
        self.assertIn("python scripts/ci/check_native_services.py --service postgres",workflow)
        self.assertIn("target/windows-postgres/installation.json",workflow)
        self.assertIn("python scripts/ci/install_windows_services.py --service s3",workflow)
        self.assertIn("python scripts/ci/check_native_services.py --service s3",workflow)
        self.assertIn("target/windows-s3/installation.json",workflow)

class ExecutionReceiptTests(unittest.TestCase):
    def make_receipts(self,base):
        receipts=[]
        for name,cases in [("one",["wake::nats_tests::nats_wake_signal_notifies_a_waiter","wake::nats_tests::nats_fixture_absence_and_configured_failure_are_distinct"]),
                           ("two",["hand::tests::run_nats_serves_a_real_tool_when_a_broker_is_reachable","hand::tests::nats_fixture_absence_and_configured_failure_are_distinct"])]:
            directory=base/"test-timings"/name;directory.mkdir(parents=True)
            doc={"status":"passed","exit_code":0,"source_unchanged":True,"source_digest":"source",
                "selected_cases":sorted(cases),"required_cases":cases,"executed_cases":[[x,"ok"] for x in cases],"arguments":[]}
            (directory/"results.json").write_text(json.dumps(doc),encoding="utf-8")
            (directory/"list.log").write_text("".join(x+": test\n" for x in cases),encoding="utf-8")
            (directory/"results.log").write_text("".join("test "+x+" ... ok\n" for x in cases)+"test result: ok. 2 passed; 0 failed; 0 ignored;\nVERIFIED live NATS fixture\n",encoding="utf-8")
            receipts.append(directory)
        return receipts

    def test_postgres_requires_all_domains_and_boolean_live_fixture_flags(self):
        import check_rust_test_execution as q
        packages=["awaken-reliability-testkit","awaken-model-catalog-store","awaken-credential-store",
            "awaken-data-subject-store","awaken-captured-content-store","awaken-work-store",
            "awaken-session-store","awaken-memory-store","awaken-skill-store","awaken-work-store","awaken-dispatch-http",
            "awaken-config-store","awaken-session-store","awaken-credential-store","awaken-cli","awaken-cli","awaken-coordinator"]
        with tempfile.TemporaryDirectory() as directory,mock.patch.object(q,"verify_preparation"):
            base=Path(directory);receipts=[]
            for index,package in enumerate(packages):
                path=base/"test-timings"/str(index);path.mkdir(parents=True);case="law_"+str(index)
                doc={"status":"passed","exit_code":0,"source_unchanged":True,"source_digest":"source",
                    "selected_cases":[case],"executed_cases":[[case,"ok"]],"required_cases":[case],
                    "arguments":["-p",package],"postgres_fixture_required":True,"postgres_fixture_configured":True}
                (path/"results.json").write_text(json.dumps(doc),encoding="utf-8")
                (path/"list.log").write_text(case+": test\n",encoding="utf-8")
                (path/"results.log").write_text("test "+case+" ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n",encoding="utf-8")
                receipts.append(path)
            log="".join("execution evidence: "+str(path)+"\n" for path in receipts)
            with mock.patch.dict(os.environ,{"CARGO_TARGET_DIR":str(base)}):
                self.assertEqual(len(gate.reconcile_postgres(log,"source")),17)
                with self.assertRaisesRegex(ValueError,"all 17"):gate.reconcile_postgres("","source")
                path=receipts[0]/"results.json";original=json.loads(path.read_text())
                for key in ("postgres_fixture_required","postgres_fixture_configured"):
                    for invalid in (False,"1"):
                        doc=dict(original);doc[key]=invalid;path.write_text(json.dumps(doc),encoding="utf-8")
                        with self.assertRaisesRegex(ValueError,"owned live fixture"):gate.reconcile_postgres(log,"source")
                path.write_text(json.dumps(original),encoding="utf-8")
                path=receipts[-2]/"results.json";doc=json.loads(path.read_text());doc["arguments"]=["-p","awaken-memory-store"]
                path.write_text(json.dumps(doc),encoding="utf-8")
                with self.assertRaisesRegex(ValueError,"omitted or duplicated"):gate.reconcile_postgres(log,"source")

    def test_actual_names_source_and_live_marker_are_all_required(self):
        import check_rust_test_execution as q
        with tempfile.TemporaryDirectory() as directory,mock.patch.object(q,"verify_preparation"):
            base=Path(directory)
            receipts=self.make_receipts(base)
            log="".join("execution evidence: "+str(p)+"\n" for p in receipts)
            with mock.patch.dict(os.environ,{"CARGO_TARGET_DIR":str(base)}):
                self.assertEqual(len(gate.reconcile_nats(log,"source")),2)
                with self.assertRaisesRegex(ValueError,"both complete"):gate.reconcile_nats("","source")
                with self.assertRaisesRegex(ValueError,"qualify this source"):gate.reconcile_nats(log,"other-source")
                path=receipts[0]/"results.log";original=path.read_text(encoding="utf-8")
                for changed in (original.replace("VERIFIED live NATS fixture",""),
                                original.replace(" ... ok"," ... ignored",1),
                                original.replace(" ... ok"," ... FAILED",1),
                                original+"SKIPPED NATS fixture\n"):
                    path.write_text(changed,encoding="utf-8")
                    with self.subTest(output=changed),self.assertRaises(ValueError):gate.reconcile_nats(log,"source")
                path.write_text(original,encoding="utf-8")
                doc_path=receipts[0]/"results.json";doc=json.loads(doc_path.read_text())
                doc["executed_cases"]=[];doc_path.write_text(json.dumps(doc),encoding="utf-8")
                with self.assertRaisesRegex(ValueError,"identities differ"):gate.reconcile_nats(log,"source")


class WorkerResultTests(unittest.TestCase):
    def run_worker(self,base,child_code=0,changed=False,cleanup_failure=False):
        import rust_test_plan
        facts={"revision":b"revision","index_tree":b"tree","patch":b"patch"}
        args=argparse.Namespace(output=base/"out",nats_bin=base/"nats.exe",service="nats",recovery=False)
        fixture=base/"fixture";fixture.mkdir()
        service=mock.Mock()
        service.start.return_value={"url":"nats://127.0.0.1:1234"}
        service.close.return_value={"stopped":True}
        if cleanup_failure:service.close.side_effect=RuntimeError("cleanup failed")
        snapshots=[("source",facts),("different" if changed else "source",facts)]
        with mock.patch.object(rust_test_plan,"working_subject",side_effect=snapshots),\
             mock.patch.object(gate.tempfile,"mkdtemp",return_value=str(fixture)),\
             mock.patch.object(gate,"NatsService",return_value=service),\
             mock.patch.object(gate.subprocess,"run",return_value=mock.Mock(returncode=child_code)),\
             mock.patch.object(gate,"reconcile_nats",return_value=[{"verified":True}]):
            code=gate.worker(args)
        return code,json.loads((base/"out/result.json").read_text(encoding="utf-8")),service

    def test_failed_child_keeps_its_status_when_cleanup_also_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            code,record,service=self.run_worker(Path(directory),17,cleanup_failure=True)
            self.assertEqual(code,17);self.assertEqual(record["status"],"failed")
            self.assertIn("cleanup_error",record);service.close.assert_called_once()

    def test_source_drift_and_cleanup_failure_each_prevent_success(self):
        for changed,cleanup_failure in [(True,False),(False,True),(False,False)]:
            with self.subTest(changed=changed,cleanup_failure=cleanup_failure),tempfile.TemporaryDirectory() as directory:
                code,record,service=self.run_worker(Path(directory),changed=changed,cleanup_failure=cleanup_failure)
                self.assertEqual(code==0,not changed and not cleanup_failure)
                self.assertEqual(record["status"]=="passed",code==0)
                service.verify.assert_called_once()


class PostgresFixtureTests(unittest.TestCase):
    def test_native_diagnostics_and_sql_have_distinct_encoding_contracts(self):
        # C1 command kind; C2 native-localized versus UTF8 output; C3 success.
        # D1/D2: initdb/pg_ctl + non-UTF8 + success -> preserve raw diagnostics;
        # S1/S2: SQL + valid UTF8 (ASCII/Unicode) -> exact stripped text;
        # S3: SQL + invalid UTF8 -> refuse, never replace or ignore bytes.
        # The existing nonzero-psql case separately requires a sanitized error.
        localized = "数据库初始化完成".encode("gb18030")
        rows = [
            ("initdb", localized, localized),
            ("pg_ctl", localized, localized),
            ("psql", b"  owned\n", "owned"),
            ("psql", ' {"owner":"工作区"}\n'.encode("utf-8"), '{"owner":"工作区"}'),
            ("psql", localized, None),
        ]
        for name, output, expected in rows:
            with self.subTest(command=name, valid_sql=expected is not None):
                service = object.__new__(pg.PostgresService)
                service.binaries = {name: {"path": "owned-" + name + ".exe"}}
                service.environment = {}
                with mock.patch.object(pg.subprocess, 'CREATE_NO_WINDOW', 0, create=True), mock.patch.object(
                    pg.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, output, b'')
                ):
                    if name != "psql":
                        self.assertEqual(service.command(name, []), expected)
                    elif expected is None:
                        with self.assertRaises(UnicodeDecodeError):
                            service.sql("SELECT 1")
                    else:
                        self.assertEqual(service.sql("SELECT 1"), expected)

    def test_sql_client_encoding_is_owned_instead_of_inherited(self):
        # C1 ambient PG client encoding/password; C2 a fresh owned fixture.
        # E1 SQL explicitly uses UTF8; E2 the ambient credential is replaced.
        # Database locale and native command diagnostics remain independent.
        inherited_password = secrets.token_hex(24)
        # This table exercises environment selection independently of the
        # native-platform/PE admission covered by the service boundary tests.
        with tempfile.TemporaryDirectory() as directory:
            binaries = Path(directory)
            fixture_root = binaries / "owned"
            with mock.patch.dict(
                os.environ, {"PGCLIENTENCODING": "LATIN1", "PGPASSWORD": inherited_password}
            ), mock.patch.object(pg.os, 'name', 'nt'), mock.patch.object(
                pg, 'windows_executable', side_effect=lambda path: {"path": str(path)}
            ):
                service = pg.PostgresService(binaries, fixture_root, binaries, "owned")
        self.assertEqual(service.environment.get("PGCLIENTENCODING"), "UTF8")
        self.assertEqual(service.environment["PGPASSWORD"], service.password)
        self.assertNotEqual(service.password, inherited_password)

    def test_sql_identity_requires_every_ownership_and_platform_premise(self):
        with tempfile.TemporaryDirectory() as directory:
            data = Path(directory) / 'data'
            value = dict(cluster_name='owned', port=1234, data_directory=str(data), started='one',
                         version='PostgreSQL 16.15, compiled by Visual C++ build 1944', version_num=160015)
            self.assertEqual(pg.validate_identity(value, 'owned', 1234, data), value)
            for changed in (dict(value, cluster_name='foreign'), dict(value, port=4321),
                            dict(value, data_directory=str(data.parent)), dict(value, started=''),
                            dict(value, version='another platform'), dict(value, version_num=170000),
                            dict(value, version_num='160015'), {}):
                with self.subTest(value=changed), self.assertRaises(ValueError):
                    pg.validate_identity(changed, 'owned', 1234, data)

    def test_control_refuses_foreign_pid_or_data_before_invoking_pg_ctl(self):
        for pid, folder in [('other', 'owned'), ('987', 'foreign'), ('987', 'owned')]:
            with self.subTest(pid=pid, folder=folder), tempfile.TemporaryDirectory() as directory:
                root = Path(directory); data = root / 'owned'; data.mkdir()
                (data / 'postmaster.pid').write_text(pid + '\n' + str(root / folder) + '\n')
                service = object.__new__(pg.PostgresService)
                service.data = data
                service.process = mock.Mock(pid=987, returncode=0)
                service.process.poll.return_value = None
                service.command = mock.Mock()
                service.log = mock.Mock()
                if pid == '987' and folder == 'owned':
                    self.assertTrue(service.close()['stopped'])
                    service.command.assert_called_once_with('pg_ctl', ['stop', '-D', str(data), '-m', 'fast', '-w', '-t', '10'], timeout=15)
                else:
                    with self.assertRaisesRegex(RuntimeError, 'ownership changed'):
                        service.close()
                    service.command.assert_not_called()
                service.log.close.assert_called_once()

    def test_initdb_failure_removes_only_its_ephemeral_password_file(self):
        with tempfile.TemporaryDirectory() as directory:
            service = object.__new__(pg.PostgresService)
            service.root = Path(directory) / 'owned'
            service.data = service.root / 'data'
            service.password = secrets.token_hex(24)
            service.command = mock.Mock(side_effect=RuntimeError('initdb failed'))
            with mock.patch.object(pg.subprocess, 'Popen') as launch:
                with self.assertRaisesRegex(RuntimeError, 'initdb failed'):
                    service.start()
                launch.assert_not_called()
            self.assertFalse((service.root / 'init-password').exists())

    def test_psql_failure_does_not_publish_connection_credentials(self):
        service = object.__new__(pg.PostgresService)
        service.binaries = {'psql': {'path': 'owned-psql.exe'}}
        credential = secrets.token_hex(24)
        service.environment = {'PGPASSWORD': credential}
        with mock.patch.object(pg.subprocess, 'CREATE_NO_WINDOW', 0, create=True), mock.patch.object(pg.subprocess, 'run', return_value=subprocess.CompletedProcess([], 17, b'', credential.encode())):
            with self.assertRaises(RuntimeError) as error:
                service.sql('SELECT 1')
        self.assertIn('17', str(error.exception))
        self.assertNotIn(credential, str(error.exception))

    def test_public_receipt_contains_no_authentication_secret(self):
        service = object.__new__(pg.PostgresService)
        service.binaries = {}; service.process = mock.Mock(pid=987)
        service.owner = 'owned'; service.identity = {}; service.port = 1234
        credential = secrets.token_hex(24)
        service.password = credential; service.url = f'postgres://user:{credential}@host/db'
        receipt = service.receipt()
        self.assertNotIn(credential, json.dumps(receipt))
        self.assertNotIn('url', receipt)


class MinioFixtureTests(unittest.TestCase):
    def test_authenticated_identity_binds_one_endpoint_and_one_owned_drive(self):
        with tempfile.TemporaryDirectory() as directory:
            data = Path(directory) / 'owned'
            value = {'status': 'success', 'info': {'deploymentID': 'one', 'servers': [
                {'state': 'online', 'endpoint': '127.0.0.1:1234', 'version': 'release', 'commitID': 'commit',
                 'drives': [{'state': 'ok', 'path': str(data), 'uuid': 'one-drive'}]}]}}
            self.assertEqual(s3.validate_minio_identity(value, '127.0.0.1:1234', data)['deployment_id'], 'one')
            cases = []
            for key, changed in [('status', 'error'), ('deploymentID', ''), ('endpoint', 'foreign:1234'),
                                 ('state', 'offline'), ('path', str(data.parent)), ('uuid', '')]:
                candidate = copy.deepcopy(value)
                if key == 'status': candidate[key] = changed
                elif key == 'deploymentID': candidate['info'][key] = changed
                elif key in ('endpoint', 'state'): candidate['info']['servers'][0][key] = changed
                else: candidate['info']['servers'][0]['drives'][0][key] = changed
                cases.append(candidate)
            candidate = copy.deepcopy(value); candidate['info']['servers'] *= 2; cases.append(candidate)
            for candidate in cases:
                with self.subTest(candidate=candidate), self.assertRaises(ValueError):
                    s3.validate_minio_identity(candidate, '127.0.0.1:1234', data)

    def test_credential_generation_advances_only_after_all_admin_operations(self):
        for failed_step in (None, 0, 1, 2):
            with self.subTest(failed_step=failed_step):
                service = object.__new__(s3.MinioService)
                service.rotated = False; service.verify = mock.Mock()
                service.user = 'application'; service.new_secret = secrets.token_hex(24)
                outcomes = [None] * 3
                if failed_step is not None: outcomes[failed_step] = RuntimeError('admin operation failed')
                service.command = mock.Mock(side_effect=outcomes)
                if failed_step is None:
                    service.rotate(); self.assertTrue(service.rotated)
                    self.assertEqual(service.verify.call_count, 2)
                    with self.assertRaisesRegex(RuntimeError, 'already applied'): service.rotate()
                else:
                    with self.assertRaisesRegex(RuntimeError, 'admin operation failed'): service.rotate()
                    self.assertFalse(service.rotated)
                    self.assertEqual(service.command.call_count, failed_step + 1)

    def test_old_and_new_clients_keep_the_same_endpoint_identity_and_rotation_id(self):
        service = object.__new__(s3.MinioService)
        service.endpoint = 'http://127.0.0.1:1234'; service.user = 'application'; service.owner = 'same-coordinate'
        service.old_secret, service.new_secret = secrets.token_hex(24), secrets.token_hex(24)
        service.rotated = False; before = service.test_environment()
        self.assertNotIn('AWAKEN_TEST_S3_OLD_SECRET_KEY', before)
        service.rotated = True; after = service.test_environment()
        self.assertEqual(after['AWAKEN_TEST_S3_OLD_SECRET_KEY'], before['AWAKEN_TEST_S3_SECRET_KEY'])
        self.assertNotEqual(after['AWAKEN_TEST_S3_SECRET_KEY'], before['AWAKEN_TEST_S3_SECRET_KEY'])
        for key in ('AWAKEN_TEST_S3_ENDPOINT', 'AWAKEN_TEST_S3_ACCESS_KEY', 'AWAKEN_TEST_S3_ROTATION_ID'):
            self.assertEqual(before[key], after[key])

    def test_public_receipt_omits_root_and_application_credentials(self):
        service = object.__new__(s3.MinioService)
        service.binaries = {}; service.process = mock.Mock(pid=987); service.owner = 'owned'
        service.identity = {}; service.endpoint = 'http://127.0.0.1:1234'; service.rotated = False
        service.root_secret, service.old_secret, service.new_secret = [secrets.token_hex(24) for _ in range(3)]
        encoded = json.dumps(service.receipt())
        for value in (service.root_secret, service.old_secret, service.new_secret):
            self.assertNotIn(value, encoded)

    def test_cleanup_failure_cannot_report_stopped(self):
        service = object.__new__(s3.MinioService)
        service.process = mock.Mock(pid=987); service.process.poll.return_value = None
        service.process.wait.side_effect = subprocess.TimeoutExpired('owned-native-minio', 10)
        service.log = mock.Mock()
        with self.assertRaises(subprocess.TimeoutExpired): service.close()
        service.process.terminate.assert_called_once()
        service.log.close.assert_called_once()



class S3ExecutionReceiptTests(unittest.TestCase):
    def make_receipts(self, base):
        phases = [
            ['object::s3_fixture::tests::required_switch_has_one_canonical_value', 'object::s3_fixture::tests::admission_table_preserves_values_and_rejects_missing_invalid_or_empty_fields'],
            ['object::tests::minio_conforms', 'object::tests::minio_conforms_fixture_admission'],
            ['object::tests::minio_rotation_seed'],
            ['object::tests::minio_rotated_credentials_revoke_old_secret_and_preserve_objects'],
        ]
        receipts=[]
        for index,cases in enumerate(phases):
            path=base/'test-timings'/str(index);path.mkdir(parents=True)
            doc={'status':'passed','exit_code':0,'source_unchanged':True,'source_digest':'source',
                 'selected_cases':sorted(cases),'required_cases':cases,'executed_cases':[[case,'ok'] for case in cases],
                 'arguments':['-p','awaken-file-store'],'s3_fixture_required':True,'s3_fixture_configured':True}
            (path/'results.json').write_text(json.dumps(doc),encoding='utf-8')
            (path/'list.log').write_text(''.join(case+': test\n' for case in cases),encoding='utf-8')
            (path/'results.log').write_text(''.join('test '+case+' ... ok\n' for case in cases)+'test result: ok. '+str(len(cases))+' passed; 0 failed; 0 ignored;\n'+('VERIFIED live S3 fixture\n' if index else ''),encoding='utf-8')
            receipts.append(path)
        return receipts

    def test_s3_requires_every_real_phase_exact_source_and_boolean_fixture_flags(self):
        import check_rust_test_execution as q
        with tempfile.TemporaryDirectory() as directory,mock.patch.object(q,'verify_preparation'):
            base=Path(directory);receipts=self.make_receipts(base)
            def references(rows):return ''.join('execution evidence: '+str(path)+'\n' for path in rows)
            log=references(receipts)
            with mock.patch.dict(os.environ,{'CARGO_TARGET_DIR':str(base)}):
                self.assertEqual(sum(len(row['selected_cases']) for row in gate.reconcile_s3(log,'source')),6)
                for rows in (receipts[:-1],[*receipts[:-1],receipts[-2]]):
                    with self.assertRaises(ValueError):gate.reconcile_s3(references(rows),'source')
                with self.assertRaises(ValueError):gate.reconcile_s3(log,'other-source')
                path=receipts[0]/'results.json';original=json.loads(path.read_text())
                for key in ('s3_fixture_required','s3_fixture_configured'):
                    for invalid in (False,'1',None):
                        doc=dict(original);doc[key]=invalid;path.write_text(json.dumps(doc),encoding='utf-8')
                        with self.subTest(key=key,invalid=invalid),self.assertRaisesRegex(ValueError,'require the owned'):
                            gate.reconcile_s3(log,'source')
                path.write_text(json.dumps(original),encoding='utf-8')
                for index in (1,2,3):
                    path=receipts[index]/'results.log';original_output=path.read_text()
                    for changed in (original_output.replace('VERIFIED live S3 fixture',''),
                                    original_output.replace(' ... ok',' ... ignored',1),
                                    original_output+'SKIPPED S3 fixture\n'):
                        path.write_text(changed,encoding='utf-8')
                        with self.subTest(phase=index),self.assertRaises(ValueError):gate.reconcile_s3(log,'source')
                    path.write_text(original_output,encoding='utf-8')

class S3InstallTests(unittest.TestCase):
    @unittest.skipUnless(os.name=='nt','native Windows installer')
    def test_direct_s3_assets_require_both_checksums_and_windows_pe_before_admission(self):
        import hashlib
        import install_windows_services as installer
        for fault in (None,'minio-checksum','mc-checksum','mc-not-pe'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
                base=Path(directory);archives=base/'archives';archives.mkdir();pins={}
                image=bytearray(90);image[:2]=b'MZ';image[60:64]=struct.pack('<I',70)
                image[70:76]=b'PE\0\0'+struct.pack('<H',0x8664)
                for name in ('minio','mc'):
                    payload=b'not-windows' if fault==name+'-not-pe' else bytes(image)
                    pins[name]={'archive':name+'.exe','url':'https://example.invalid/'+name,
                        'bytes':len(payload),'sha256':hashlib.sha256(payload).hexdigest(),'executable':name+'.exe'}
                    (archives/(name+'.exe')).write_bytes(payload)
                    if fault==name+'-checksum':pins[name]['sha256']='0'*64
                with mock.patch.dict(installer.PINS,pins),mock.patch.object(installer,'extract_runtime') as extract:
                    if fault is None:
                        report=installer.install(base/'out',archives,'s3')
                        self.assertEqual(report['status'],'passed');self.assertEqual(set(report['services']),{'minio','mc'})
                    else:
                        with self.assertRaises(ValueError):installer.install(base/'out',archives,'s3')
                        self.assertEqual(json.loads((base/'out/installation.json').read_text())['status'],'failed')
                    extract.assert_not_called()

    def test_unknown_zip_kind_cannot_fall_through_to_postgres_extraction(self):
        import install_windows_services as installer
        with self.assertRaisesRegex(ValueError,'unsupported'):
            installer.extract_runtime(Path('not-opened.zip'),Path('not-written'),'s3')


if __name__=="__main__":
    unittest.main(verbosity=2)
