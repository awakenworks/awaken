#!/usr/bin/env python3
"""Require real local Windows services and reconcile actual test execution."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import uuid

from native_services import NatsService, sha256

ROOT=Path(__file__).resolve().parents[2]


def parser() -> argparse.ArgumentParser:
    p=argparse.ArgumentParser(allow_abbrev=False)
    p.add_argument("--service",choices=["nats"],required=True)
    p.add_argument("--nats-bin",type=Path,required=True)
    p.add_argument("--output",type=Path,required=True)
    p.add_argument("--timeout",type=int,default=7200)
    p.add_argument("--owned-worker",action="store_true",help=argparse.SUPPRESS)
    return p


def reconcile_nats(log: str, source_digest: str) -> list[dict]:
    import check_rust_test_execution as q
    references=re.findall(r"^execution evidence: (.+)$",log,re.M)
    if len(references)!=2:
        raise ValueError("NATS gate must return both complete domain receipts")
    receipts=[]
    floor={"wake::nats_tests::nats_wake_signal_notifies_a_waiter",
        "hand::tests::run_nats_serves_a_real_tool_when_a_broker_is_reachable"}
    observed=set()
    for reference in references:
        value=reference.strip()
        if re.match(r"^/[a-zA-Z]/",value): value=value[1]+":"+value[2:]
        directory=Path(value).resolve(strict=True)
        expected_parent=Path(os.environ.get("CARGO_TARGET_DIR",str(ROOT/"target"))).resolve()/"test-timings"
        if directory.parent!=expected_parent or directory.is_symlink():
            raise ValueError("NATS receipt is outside the current Cargo evidence directory")
        document=json.loads((directory/"results.json").read_text(encoding="utf-8"))
        if (document.get("status")!="passed" or document.get("exit_code")!=0
                or not document.get("source_unchanged") or document.get("source_digest")!=source_digest):
            raise ValueError("NATS execution receipt does not qualify this source")
        results=(directory/"results.log").read_text(encoding="utf-8",errors="replace")
        actual=q.execution_evidence((directory/"list.log").read_text(encoding="utf-8"),results,0,document["required_cases"])
        if (actual["errors"] or actual["selected_cases"]!=document["selected_cases"]
                or [list(row) for row in actual["executed_cases"]]!=document.get("executed_cases")):
            raise ValueError("NATS execution identities differ from the inventory")
        if "VERIFIED live NATS fixture" not in results:
            raise ValueError("NATS domain did not report an actual live protocol assertion")
        q.verify_preparation(directory/"build.json",document["arguments"],source_digest)
        observed.update(document["selected_cases"])
        receipts.append({"directory":str(directory),"receipt_sha256":sha256(directory/"results.json"),
            "selected_cases":document["selected_cases"]})
    if not floor<=observed:
        raise ValueError("NATS gate omitted a required protocol law")
    return receipts


def worker(args: argparse.Namespace) -> int:
    from rust_test_plan import working_subject
    digest,facts=working_subject()
    out=args.output.resolve()
    out.mkdir(parents=True,exist_ok=False)
    record={"schema_version":1,"status":"running","platform":"Windows/MSVC",
        "service":args.service,"linux_verification_allowed":False,"full_gate":False,
        "source":{"revision":facts["revision"].decode().strip(),"index_tree":facts["index_tree"].decode().strip(),
            "digest":digest,"patch_sha256":hashlib.sha256(facts["patch"]).hexdigest()},
        "process_ownership":"native Windows deadline Job with kill-on-close","stages":[]}
    (out/"source.patch").write_bytes(facts["patch"])
    def save():
        pending=out/"result.pending"
        pending.write_text(json.dumps(record,indent=2),encoding="utf-8")
        pending.replace(out/"result.json")
    save()
    root=Path(tempfile.mkdtemp(prefix="awaken-services."))
    owner="awaken-ci-"+uuid.uuid4().hex
    (root/"owner.json").write_text(json.dumps({"owner":owner,"evidence":str(out)}),encoding="utf-8")
    record.update(fixture_root=str(root),fixture_data_retained=True)
    service=None
    code=0
    try:
        service=NatsService(args.nats_bin,root/"nats",out,owner)
        record["fixture"]=service.start()
        save()
        environment=dict(os.environ,AWAKEN_TEST_NATS_URL=record["fixture"]["url"],AWAKEN_TEST_REQUIRE_NATS="1")
        # Never borrow an unrelated database or another service fixture.
        for name in ("AWAKEN_TEST_DATABASE_URL","AWAKEN_TEST_REQUIRE_POSTGRES"):
            environment.pop(name,None)
        command=[sys.executable,"-X","utf8","scripts/ci/_native_shell.py","scripts/ci/nats_tests.sh"]
        start=time.monotonic()
        with (out/"tests.log").open("wb") as log:
            child=subprocess.run(command,cwd=ROOT,env=environment,stdout=log,stderr=subprocess.STDOUT,stdin=subprocess.DEVNULL)
        record["stages"].append({"stage":"nats-domain-tests","command":command,"exit_code":child.returncode,
            "seconds":time.monotonic()-start,"log_sha256":sha256(out/"tests.log")})
        save()
        if child.returncode:
            code=child.returncode
            raise RuntimeError("native NATS domain tests failed; inspect tests.log")
        service.verify()
        record["execution"]=reconcile_nats((out/"tests.log").read_text(encoding="utf-8",errors="replace"),digest)
        if working_subject()[0]!=digest:
            raise RuntimeError("source changed during native service verification")
        record["source_unchanged"]=True
    except Exception as error:
        code=code or 1
        record["error"]=str(error)
    finally:
        try:
            if service is not None: record["cleanup"]=service.close()
        except Exception as error:
            code=code or 1
            record["cleanup_error"]=str(error)
        record.update(status="passed" if code==0 else "failed",exit_code=code)
        save()
    print(json.dumps({k:record[k] for k in ("status","service","platform","exit_code")}))
    return code


def main(argv: list[str]) -> int:
    args=parser().parse_args(argv)
    if os.name!="nt":
        raise SystemExit("native service validation requires Windows; no container fallback")
    if args.timeout<=0:
        raise SystemExit("native service timeout must be positive")
    if args.owned_worker: return worker(args)
    from deadline import run
    # Establish the native shell environment once, before binding the source
    # and build context. Nested domain scripts inherit the same HOME/tool
    # identities; PowerShell and MSYS need not fabricate equivalent contexts.
    result=run(args.timeout,[sys.executable,"-X","utf8",str(ROOT/"scripts/ci/_native_shell.py"),
        "scripts/ci/_provider_environment.sh","--exec",sys.executable,"-X","utf8",
        str(Path(__file__).resolve()),*argv,"--owned-worker"])
    if result==0:
        report=json.loads((args.output/"result.json").read_text(encoding="utf-8"))
        if report.get("status")!="passed" or not report.get("cleanup",{}).get("stopped"):
            return 1
    return result


if __name__=="__main__":
    raise SystemExit(main(sys.argv[1:]))
