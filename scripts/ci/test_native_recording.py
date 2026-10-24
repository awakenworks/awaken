#!/usr/bin/env python3
"""Fault tables for atomic native records and preserving the actual child cause."""
from __future__ import annotations

import contextlib
import copy
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import check_native_services as gate
import rust_test_plan

INPUT_GLOBS = ("scripts/ci/**", ".github/workflows/windows-native-services.yml")


class NativeRecordingTests(unittest.TestCase):
    def test_atomic_record_replacement_preserves_the_previous_record_on_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            gate._save_record(root,{"status":"running"})
            before=(root/"result.json").read_bytes()
            with mock.patch.object(Path,"replace",side_effect=OSError("controlled atomic replacement failure")):
                with self.assertRaises(OSError):gate._save_record(root,{"status":"passed"})
            self.assertEqual((root/"result.json").read_bytes(),before)
            with self.assertRaises(TypeError):gate._save_record(root,{"status":object()})
            self.assertEqual((root/"result.json").read_bytes(),before)
            gate._save_record(root,{"status":"failed","exit_code":17})
            self.assertEqual(json.loads((root/"result.json").read_text()),{"status":"failed","exit_code":17})
            self.assertFalse((root/"result.pending").exists())

    def fixture(self,root,child_status):
        fixture=root/"fixture";fixture.mkdir()
        args=gate.parser().parse_args(["--service","nats","--nats-bin",str(root/"unused.exe"),"--output",str(root/"out")])
        service=mock.Mock()
        service.start.return_value={"url":"nats://127.0.0.1:1","owner":"owned"}
        service.close.return_value={"stopped":True}
        facts={"revision":b"revision","index_tree":b"tree","patch":b"patch"}
        stack=contextlib.ExitStack()
        stack.enter_context(mock.patch.object(rust_test_plan,"working_subject",return_value=("source",facts)))
        stack.enter_context(mock.patch.object(gate.tempfile,"mkdtemp",return_value=str(fixture)))
        stack.enter_context(mock.patch.object(gate,"NatsService",return_value=service))
        stack.enter_context(mock.patch.object(gate.subprocess,"run",return_value=mock.Mock(returncode=child_status)))
        stack.enter_context(mock.patch.object(gate,"reconcile_nats",return_value=[{"verified":True}]))
        return stack,args,service

    def test_child_status_survives_hash_stage_terminal_and_cleanup_faults(self):
        # Every decision is isolated from filesystem timing. The atomic writer
        # above and live native lanes exercise real IO; this table owns causes.
        for phase in ("hash","stage","terminal"):
            for child_status in (0,17):
                for error_type in (OSError,TypeError):
                    for cleanup_fails in (False,True):
                        with self.subTest(phase=phase,status=child_status,error=error_type,cleanup=cleanup_fails),tempfile.TemporaryDirectory() as directory:
                            stack,args,service=self.fixture(Path(directory),child_status)
                            snapshots=[];faults=[]
                            if cleanup_fails:service.close.side_effect=RuntimeError("owned cleanup failed")
                            def fail():
                                faults.append(phase);raise error_type("controlled "+phase+" record fault")
                            def save(_output,value):
                                if not faults and ((phase=="stage" and value["stages"]) or (phase=="terminal" and value["status"]!="running")):fail()
                                snapshots.append(copy.deepcopy(value))
                            def digest(_path):
                                if phase=="hash" and not faults:fail()
                                return "0"*64
                            with stack,mock.patch.object(gate,"_save_record",side_effect=save),mock.patch.object(gate,"sha256",side_effect=digest),contextlib.redirect_stdout(io.StringIO()),contextlib.redirect_stderr(io.StringIO()):
                                status=gate.worker(args)
                            self.assertEqual(faults,[phase])
                            self.assertEqual(status,child_status or 1,snapshots)
                            value=snapshots[-1]
                            self.assertEqual(value["status"],"failed",value)
                            self.assertEqual(value["exit_code"],child_status or 1,value)
                            service.close.assert_called_once()
                            if cleanup_fails:self.assertIn("cleanup_error",value)

    def test_persistent_terminal_write_failure_cannot_pass_or_hide_child_failure(self):
        for child_status in (0,17):
            with self.subTest(status=child_status),tempfile.TemporaryDirectory() as directory:
                stack,args,service=self.fixture(Path(directory),child_status)
                snapshots=[];terminal=[]
                def save(_output,value):
                    if value["status"]!="running":
                        terminal.append(copy.deepcopy(value));raise OSError("persistent terminal failure")
                    snapshots.append(copy.deepcopy(value))
                with stack,mock.patch.object(gate,"_save_record",side_effect=save),mock.patch.object(gate,"sha256",return_value="0"*64),contextlib.redirect_stdout(io.StringIO()),contextlib.redirect_stderr(io.StringIO()):
                    status=gate.worker(args)
                self.assertEqual(status,child_status or 1)
                self.assertEqual(len(terminal),2,"one final write and at most one FAILED diagnostic write")
                self.assertEqual(terminal[-1]["status"],"failed")
                self.assertEqual(terminal[-1]["exit_code"],child_status or 1)
                self.assertTrue(all(row["status"]=="running" for row in snapshots))
                service.close.assert_called_once()

    def test_interruption_cannot_record_pass_and_original_status_survives_cleanup(self):
        for cause,status in ((KeyboardInterrupt(),130),(SystemExit(29),29),(SystemExit(0),1),(GeneratorExit(),1)):
            for cleanup_fails in (False,True):
                with self.subTest(cause=type(cause).__name__,cleanup=cleanup_fails),tempfile.TemporaryDirectory() as directory:
                    stack,args,service=self.fixture(Path(directory),0)
                    if cleanup_fails:service.close.side_effect=RuntimeError("controlled shutdown failure")
                    with stack,mock.patch.object(gate.subprocess,"run",side_effect=cause),contextlib.redirect_stdout(io.StringIO()):
                        self.assertEqual(gate.worker(args),status)
                    value=json.loads((args.output/"result.json").read_text())
                    self.assertEqual(value["status"],"failed")
                    self.assertEqual(value["exit_code"],status)
                    self.assertIsNot(value.get("source_unchanged"),True)
                    service.close.assert_called_once()

    def test_initial_record_failure_refuses_before_service_construction(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);args=gate.parser().parse_args(["--service","nats","--nats-bin","unused","--output",str(root/"out")])
            facts={"revision":b"revision","index_tree":b"tree","patch":b"patch"}
            with mock.patch.object(rust_test_plan,"working_subject",return_value=("source",facts)),mock.patch.object(gate,"_save_record",side_effect=OSError("initial evidence unavailable")),mock.patch.object(gate,"NatsService") as service,mock.patch.object(gate.subprocess,"run") as child:
                with self.assertRaises(OSError):gate.worker(args)
                service.assert_not_called();child.assert_not_called()


if __name__=="__main__":
    unittest.main(verbosity=2)
