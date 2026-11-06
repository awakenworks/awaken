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
from _native_postgres import PostgresService
from _native_postgres_gap_policy import postgres_product_gap_report, postgres_work_limitations
from _native_postgres_scopes import POSTGRES_SELECTORS, postgres_phases, postgres_plan, require_session_capacity_result
from _native_s3 import MinioService
from _native_recovery_postgres import ArchivePrimary
from _native_recovery_drill import run_recovery

ROOT=Path(__file__).resolve().parents[2]


def parser() -> argparse.ArgumentParser:
    p=argparse.ArgumentParser(allow_abbrev=False)
    p.add_argument("--service",choices=["nats", "postgres", "s3"],required=True)
    p.add_argument("--nats-bin",type=Path,help="native Windows NATS server executable")
    p.add_argument("--postgres-scope",choices=sorted(POSTGRES_SELECTORS),default="core",
                   help="required domain group on the owned PostgreSQL fixture")
    p.add_argument("--postgres-bin",type=Path,help="directory of native Windows PostgreSQL 16 bin tools")
    p.add_argument("--minio-bin",type=Path,help="native Windows MinIO executable")
    p.add_argument("--mc-bin",type=Path,help="native Windows MinIO admin client executable")
    p.add_argument("--output",type=Path,required=True)
    p.add_argument("--recovery",action="store_true",help="require physical target-LSN recovery through native S3 on this same PostgreSQL fixture")
    p.add_argument("--timeout",type=int,default=7200)
    p.add_argument("--owned-worker",action="store_true",help=argparse.SUPPRESS)
    return p


def reconcile_receipts(log: str, source_digest: str, *, service: str, fixture_owner: str, postgres_scope: str = "core") -> list[dict]:
    import nextest_execution as q
    from collections import Counter
    references=re.findall(r"^execution evidence: (.+)$",log,re.M)
    if service not in ("nats", "postgres", "s3"): raise ValueError("unsupported native service receipt kind")
    if not re.fullmatch(r"[A-Za-z0-9._:-]{1,160}", fixture_owner):
        raise ValueError("native reconciliation requires the current owned fixture")
    phases=postgres_phases(postgres_scope,ROOT) if service == "postgres" else ()
    postgres = service == "postgres"
    s3 = service == "s3"
    expected_count=len(phases) if postgres else {"nats":2,"s3":4}[service]
    if len(references)!=expected_count:
        if s3: raise ValueError("S3 gate must return all four phase receipts")
        raise ValueError(f"PostgreSQL {postgres_scope} gate must return all {expected_count} required domain receipts" if postgres
                         else "NATS gate must return both complete domain receipts")
    receipts=[]
    observed=set()
    packages=[]
    directories=set()
    for ordinal,reference in enumerate(references):
        value=reference.strip()
        if re.match(r"^/[a-zA-Z]/",value): value=value[1]+":"+value[2:]
        declared=Path(value)
        if declared.is_symlink() or declared.is_junction():
            raise ValueError("native service receipt must not be a linked directory")
        directory=declared.resolve(strict=True)
        if directory in directories:
            raise ValueError("native service gate duplicated a phase receipt directory")
        directories.add(directory)
        expected_parent=Path(os.environ.get("CARGO_TARGET_DIR",str(ROOT/"target"))).resolve()/"test-timings"
        if directory.parent!=expected_parent or directory.is_symlink():
            raise ValueError("native service receipt is outside the current Cargo evidence directory")
        receipt_bytes=(directory/"results.json").read_bytes()
        document=json.loads(receipt_bytes)
        if (document.get("status")!="passed" or document.get("exit_code")!=0
                or document.get("source_unchanged") is not True or document.get("source_digest")!=source_digest):
            raise ValueError("native service execution receipt does not qualify this source")
        runtime=document.get("execution_inputs")
        if not isinstance(runtime,dict) or runtime.get("fixture")!={"kind":service,"owner":fixture_owner}:
            raise ValueError("native service execution does not bind the current owned fixture")
        if postgres:
            phase=phases[ordinal]
            expected=q.required_command(list(phase.arguments))
            actual_signature=q.required_command(document["arguments"])
            if actual_signature!=expected:
                raise ValueError("PostgreSQL gate omitted or duplicated a canonical backend phase: "+phase.identity)
            if document.get("required_cases")!=expected[1]:
                raise ValueError("PostgreSQL receipt changed the canonical required case floors: "+phase.identity)
            if runtime.get("crash_backend")!=dict(phase.environment).get("AWAKEN_CRASH_BACKEND"):
                raise ValueError("PostgreSQL phase selected the wrong runtime backend: "+phase.identity)
        if postgres and not (document.get("postgres_fixture_required") is True and document.get("postgres_fixture_configured") is True):
            raise ValueError("PostgreSQL execution did not require the owned live fixture")
        actual=q.qualify((directory/"list.json").read_text(encoding="utf-8"),
                         (directory/"junit.xml").read_text(encoding="utf-8"),0,document["required_cases"])
        if actual["errors"] or actual["selected_cases"]!=document["selected_cases"]:
            raise ValueError("native service execution identities differ from the inventory")
        results=actual["output"]
        if s3 and not (document.get("s3_fixture_required") is True and document.get("s3_fixture_configured") is True):
            raise ValueError("S3 execution did not require the owned live fixture")
        if service == "nats" and "VERIFIED live NATS fixture" not in results:
            raise ValueError("NATS domain did not report an actual live protocol assertion")
        observed.update(document["selected_cases"])
        arguments=document["arguments"]
        if postgres or s3: packages.append(arguments[arguments.index("-p")+1])
        if s3 and "object::tests::minio_conforms" in document["selected_cases"] and "VERIFIED live S3 fixture" not in results:
            raise ValueError("S3 conformance did not report actual live protocol assertions")
        if s3 and any("minio_rotation" in case or "minio_rotated" in case for case in document["selected_cases"]) and "VERIFIED live S3 fixture" not in results:
            raise ValueError("S3 credential phase did not report actual live protocol assertions")
        if (directory/"results.json").read_bytes()!=receipt_bytes:
            raise ValueError("native service receipt changed during verification")
        row={"directory":str(directory),"receipt_sha256":hashlib.sha256(receipt_bytes).hexdigest(),
            "selected_cases":document["selected_cases"]}
        if postgres:
            capacity=require_session_capacity_result(phase,results)
            if capacity is not None: row["capacity_observation"]=capacity
        receipts.append(row)
    if s3:
        expected = [frozenset(("object::s3_fixture::tests::required_switch_has_one_canonical_value",
            "object::s3_fixture::tests::admission_table_preserves_values_and_rejects_missing_invalid_or_empty_fields")),
            frozenset(("object::tests::minio_conforms", "object::tests::minio_conforms_fixture_admission")),
            frozenset(("object::tests::minio_rotation_seed",)),
            frozenset(("object::tests::minio_rotated_credentials_revoke_old_secret_and_preserve_objects",))]
        if Counter(frozenset(row["selected_cases"]) for row in receipts) != Counter(expected) or packages != ["awaken-file-store"] * 4:
            raise ValueError("S3 gate omitted or duplicated a required credential/conformance phase")
    elif service == "nats" and not {"wake::nats_tests::nats_wake_signal_notifies_a_waiter",
            "hand::tests::run_nats_serves_a_real_tool_when_a_broker_is_reachable"}<=observed:
        raise ValueError("NATS gate omitted a required protocol law")
    return receipts


def reconcile_nats(log: str, source_digest: str, *, fixture_owner: str) -> list[dict]:
    return reconcile_receipts(log,source_digest,service="nats",fixture_owner=fixture_owner)


def reconcile_postgres(log: str, source_digest: str, scope: str = "core", *, fixture_owner: str) -> list[dict]:
    return reconcile_receipts(log,source_digest,service="postgres",postgres_scope=scope,fixture_owner=fixture_owner)


def reconcile_s3(log: str, source_digest: str, *, fixture_owner: str) -> list[dict]:
    return reconcile_receipts(log,source_digest,service="s3",fixture_owner=fixture_owner)


def _save_record(output: Path, record: dict) -> None:
    pending=output/"result.pending"
    pending.write_text(json.dumps(record,indent=2),encoding="utf-8")
    pending.replace(output/"result.json")


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
        _save_record(out,record)
    save()
    root=Path(tempfile.mkdtemp(prefix="awaken-services."))
    owner="awaken-ci-"+uuid.uuid4().hex
    (root/"owner.json").write_text(json.dumps({"owner":owner,"evidence":str(out)}),encoding="utf-8")
    record.update(fixture_root=str(root),fixture_data_retained=True)
    service=None
    recovery_objects=None
    code=0
    complete=False
    try:
        if args.service=="nats":
            service=NatsService(args.nats_bin,root/"nats",out,owner)
            plan=[("nats-domain-tests","tests.log",["scripts/ci/nats_tests.sh"])]
        elif args.service=="postgres":
            if args.recovery:
                archive=root/"wal";archive.mkdir()
                service=ArchivePrimary(args.postgres_bin,root/"postgres",out,owner,archive)
                recovery_objects=MinioService(args.minio_bin,args.mc_bin,root/"recovery-objects",out,owner,
                    ROOT/"scripts/ci/fixtures/minio-file-store-policy.json")
            else:
                service=PostgresService(args.postgres_bin,root/"postgres",out,owner)
            plan=postgres_plan(args.postgres_scope)
            record["postgres_scope"]=args.postgres_scope
            record["unverified_product_gaps"]=postgres_product_gap_report(args.postgres_scope,ROOT)
            record["unaccepted_limitations"]=postgres_work_limitations(args.postgres_scope,ROOT)
        elif args.service=="s3":
            service=MinioService(args.minio_bin,args.mc_bin,root/"minio",out,owner,
                ROOT/"scripts/ci/fixtures/minio-file-store-policy.json")
            plan=[("s3-"+phase,phase+".log",["scripts/ci/object_store_tests.sh","--"+phase])
                for phase in ("admission","conformance","seed","verify")]
        else:
            raise ValueError("unsupported native service")
        record["fixture"]=service.start()
        record["recovery_required"]=args.recovery
        save()
        if recovery_objects is not None:
            record["recovery_object_fixture"]=recovery_objects.start()
            save()
        environment=dict(os.environ)
        for name in list(environment):
            if name.upper().startswith(("AWAKEN_TEST_", "AWS_", "MINIO_", "MC_HOST_")):
                environment.pop(name,None)
        environment.pop("AWAKEN_CRASH_BACKEND",None)
        environment.update(AWAKEN_TEST_FIXTURE_KIND=args.service,AWAKEN_TEST_FIXTURE_OWNER=owner)
        record["fixture_subject"]={"kind":args.service,"owner":owner}
        if args.service=="nats":
            environment.update(AWAKEN_TEST_NATS_URL=record["fixture"]["url"],AWAKEN_TEST_REQUIRE_NATS="1")
        elif args.service=="postgres":
            environment.update(AWAKEN_TEST_DATABASE_URL=service.url,AWAKEN_TEST_REQUIRE_POSTGRES="1")
        else:
            environment.update(AWS_EC2_METADATA_DISABLED="true")
        logs=[]
        for stage,filename,arguments in plan:
            if args.service=="s3":
                if stage=="s3-verify": service.rotate()
                environment.update(service.test_environment())
            command=[sys.executable,"-X","utf8","scripts/ci/_native_shell.py",*arguments]
            start=time.monotonic()
            with (out/filename).open("wb") as log:
                child=subprocess.run(command,cwd=ROOT,env=environment,stdout=log,stderr=subprocess.STDOUT,stdin=subprocess.DEVNULL)
                # Retain the process cause before log close/hash or record IO.
                if child.returncode:
                    code=child.returncode
            record["stages"].append({"stage":stage,"command":command,"exit_code":child.returncode,
                "seconds":time.monotonic()-start,"log_sha256":sha256(out/filename)})
            save()
            if child.returncode:
                raise RuntimeError("native service domain tests failed; inspect "+filename)
            logs.append((out/filename).read_text(encoding="utf-8",errors="replace"))
        service.verify()
        if args.service=="s3":
            record["final_fixture"]=service.receipt()
            record["credential_rotation_applied"]=service.rotated
        reconcile={"nats":reconcile_nats,"postgres":reconcile_postgres,"s3":reconcile_s3}[args.service]
        record["execution"]=(reconcile_postgres("\n".join(logs),digest,args.postgres_scope,fixture_owner=owner)
                             if args.service=="postgres" else reconcile("\n".join(logs),digest,fixture_owner=owner))
        if recovery_objects is not None:
            record["recovery"]=run_recovery(service,recovery_objects,root/"physical-recovery",out)
        if working_subject()[0]!=digest:
            raise RuntimeError("source changed during native service verification")
        record["source_unchanged"]=True
        complete=True
    except BaseException as error:
        code=code or (130 if isinstance(error,KeyboardInterrupt) else error.code if isinstance(error,SystemExit) and isinstance(error.code,int) and error.code else 1)
        record["error"]=str(error)
        record["error_kind"]=type(error).__name__
    finally:
        try:
            if recovery_objects is not None: record["recovery_object_cleanup"]=recovery_objects.close()
        except BaseException as error:
            code=code or 1
            record["recovery_object_cleanup_error"]=str(error)
        try:
            if service is not None: record["cleanup"]=service.close()
        except BaseException as error:
            code=code or 1
            record["cleanup_error"]=str(error)
        code=code or int(not complete)
        record.update(status="passed" if code==0 else "failed",exit_code=code)
        try:
            save()
        except BaseException as error:
            code=code or 1
            record.update(status="failed",exit_code=code,recording_error=str(error))
            # A bounded best-effort FAILED record may preserve diagnostics after
            # a transient write fault. It never converts that fault into pass.
            try:
                save()
            except BaseException:
                pass
            print("native service terminal evidence could not be written",file=sys.stderr)
    print(json.dumps({k:record[k] for k in ("status","service","platform","exit_code")}))
    return code


def main(argv: list[str]) -> int:
    p=parser()
    args=p.parse_args(argv)
    if args.postgres_scope!="core" and args.service!="postgres": p.error("--postgres-scope requires postgres")
    if args.service=="nats" and args.nats_bin is None: p.error("nats requires --nats-bin")
    if args.service=="postgres" and args.postgres_bin is None: p.error("postgres requires --postgres-bin")
    if args.service=="s3" and (args.minio_bin is None or args.mc_bin is None): p.error("s3 requires --minio-bin and --mc-bin")
    if args.recovery and (args.service!="postgres" or args.minio_bin is None or args.mc_bin is None):
        p.error("--recovery requires postgres and both --minio-bin and --mc-bin")
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
        if report.get("status")!="passed" or report.get("cleanup",{}).get("stopped") is not True:
            return 1
        if args.recovery and (report.get("recovery_required") is not True
                or report.get("recovery",{}).get("status")!="passed"
                or report.get("recovery",{}).get("cleanup",{}).get("stopped") is not True
                or report.get("recovery_object_cleanup",{}).get("stopped") is not True):
            return 1
    return result


if __name__=="__main__":
    raise SystemExit(main(sys.argv[1:]))
