#!/usr/bin/env python3
"""Decision tables for owned native service processes and truthful receipts."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile
import unittest
from unittest import mock

import check_native_services as gate
import native_services as services

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
        args=argparse.Namespace(output=base/"out",nats_bin=base/"nats.exe",service="nats")
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


if __name__=="__main__":
    unittest.main(verbosity=2)
