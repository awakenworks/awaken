#!/usr/bin/env python3
"""Require three real HTTP/PostgreSQL scenarios on owned native Windows processes."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import uuid

from _native_postgres import PostgresService
from native_services import sha256, windows_executable
from _native_worker_fixtures import (
    WORKER_FIXTURE_CASES, discover_fixture_tests, validate_worker_fixture_probe,
)

ROOT=Path(__file__).resolve().parents[2]
CASES=(
    ("durable-commit","durable_pg_commit_e2e.mjs","DURABLE PG COMMIT E2E PASS:"),
    ("notify-wake","durable_pg_wake_e2e.mjs","DURABLE PG WAKE E2E PASS:"),
    ("claimed-commit-guard","postgres_claimed_commit_guard_e2e.ts","POSTGRES CLAIMED-COMMIT GUARD TS E2E PASS:"),
)
SKIP=re.compile(r"^[ \t]*(?:skip(?:ped|ping)?\b|\[skip\])",re.M|re.I)
SQL_CASE="Postgres observation preserves rows, quoted identity, bytes, and read-only authority"


FIXTURE_TESTS=discover_fixture_tests(ROOT)

def save_record(output: Path, record: dict) -> None:
    pending=output/"result.pending";pending.write_text(json.dumps(record,indent=2),encoding="utf-8")
    pending.replace(output/"result.json")

def validate_case(text: str, code: int, marker: str) -> None:
    if code or SKIP.search(text) or sum(line.startswith(marker) for line in text.splitlines())!=1:
        raise ValueError("required served PostgreSQL scenario did not complete its actual assertions")


def validate_sql_probe(text: str, code: int) -> None:
    if code or SKIP.search(text) or re.findall(r"^ok 1 - (.+)$",text,re.M)!=[SQL_CASE]:
        raise ValueError("required native SQL observation did not execute its actual law")
    for name,count in (("tests",1),("pass",1),("fail",0),("cancelled",0),("skipped",0)):
        if re.findall(r"^# "+name+r" (\d+)$",text,re.M)!=[str(count)]:
            raise ValueError("required native SQL observation count is incomplete")

def validate_bundle(path: Path, digest: str) -> dict:
    value=json.loads(path.read_text(encoding="utf-8"))
    if value.get("status")!="built" or value.get("source_digest")!=digest or set(value.get("binaries",{}))!={"production","scenario","hand"}:
        raise ValueError("native executable bundle does not bind this source and every required role")
    paths=set()
    for role,row in value["binaries"].items():
        executable=Path(row["path"])
        if not executable.is_absolute() or executable.is_symlink() or executable.resolve().parent!=(path.parent/"binaries").resolve():
            raise ValueError("native executable is outside the owned final snapshot")
        if executable.resolve() in paths:raise ValueError("native executable roles must be distinct")
        paths.add(executable.resolve())
        actual=windows_executable(executable)
        if actual["sha256"]!=row.get("sha256"):raise ValueError("native executable snapshot changed")
    return value["binaries"]

def case_database_name(case: str) -> str:
    if case not in {row[0] for row in CASES}:
        raise ValueError("unknown native PostgreSQL scenario")
    return "awaken_e2e_" + case.replace("-", "_")


def case_environment(inherited: dict, url: str, temporary: Path, binaries: dict, psql: Path) -> dict:
    result={k:v for k,v in inherited.items() if not k.upper().startswith(("AWAKEN_","SESSION_","PG","AWS_","MINIO_","MC_HOST_"))}
    if "AWAKEN_NATIVE_BASH" in inherited:result["AWAKEN_NATIVE_BASH"]=inherited["AWAKEN_NATIVE_BASH"]
    with socket.socket() as server,socket.socket() as worker:
        server.bind(("127.0.0.1",0));worker.bind(("127.0.0.1",0))
        ports=(server.getsockname()[1],worker.getsockname()[1])
    result.update(SESSION_DEPLOYMENT_DATABASE_URL=url,TEMP=str(temporary),TMP=str(temporary),TMPDIR=str(temporary),
        E2E_PORT=str(ports[0]),E2E_WORKER_PORT=str(ports[1]),
        AWAKEN_E2E_AWAKEN_BIN=binaries["production"]["path"],AWAKEN_E2E_SCENARIO_HOST_BIN=binaries["scenario"]["path"],
        PATH=str(psql.parent)+os.pathsep+inherited.get("PATH",""))
    return result


def reconcile_execution(output: Path) -> dict:
    from rust_test_plan import working_subject
    value=json.loads((output/"result.json").read_text(encoding="utf-8"))
    if (value.get("status")!="passed" or value.get("exit_code")!=0 or value.get("source_unchanged") is not True
            or value.get("cleanup",{}).get("stopped") is not True or value["cleanup"].get("exit_code")!=0
            or [row.get("case") for row in value.get("cases",[])]!=[row[0] for row in CASES]):
        raise ValueError("native served PostgreSQL receipt is incomplete")
    if (value.get("source",{}).get("digest")!=working_subject()[0]
            or value["source"].get("patch_sha256")!=sha256(output/"source.patch")):
        raise ValueError("native served PostgreSQL receipt does not bind the current source")
    stages=value.get("stages",[])
    if [row.get("stage") for row in stages]!=["disk-admission","rustc-identity","node-identity","node-dependencies","worker-fixture-codec","build-execution-bundles","psql-observation",*[row[0] for row in CASES]]:
        raise ValueError("native served PostgreSQL preparation or case inventory changed")
    for row in stages:
        log=output/(row["stage"]+".log")
        if row.get("exit_code")!=0 or row.get("log_sha256")!=sha256(log):raise ValueError("native served PostgreSQL log changed")
    fixture_probe=value.get("worker_fixture_probe",{})
    if (fixture_probe.get("status")!="passed" or fixture_probe.get("sources")!={path:sha256(ROOT/path) for path in FIXTURE_TESTS}):
        raise ValueError("native Worker fixture codec source receipt changed")
    actual=validate_worker_fixture_probe((output/"worker-fixture-codec.log").read_text(encoding="utf-8",errors="replace"),0)
    if actual!=fixture_probe.get("cases"):raise ValueError("native Worker fixture codec outcomes changed")
    probe=value.get("sql_probe",{})
    if (probe.get("status")!="passed" or probe.get("source")!="e2e/postgres.test.mjs"
            or probe.get("source_sha256")!=sha256(ROOT/"e2e/postgres.test.mjs")):
        raise ValueError("native SQL observation receipt changed")
    validate_sql_probe((output/"psql-observation.log").read_text(encoding="utf-8",errors="replace"),0)
    for (case,filename,marker),row in zip(CASES,value["cases"],strict=True):
        if (row.get("status")!="passed" or row.get("temporary_removed") is not True
                or row.get("database")!=case_database_name(case)
                or row.get("source")!="e2e/"+filename or row.get("source_sha256")!=sha256(ROOT/"e2e"/filename)):
            raise ValueError("native served PostgreSQL case receipt changed")
        validate_case((output/(case+".log")).read_text(encoding="utf-8",errors="replace"),0,marker)
    root=Path(value["fixture_root"]);owner=json.loads((root/"owner.json").read_text(encoding="utf-8"))
    if owner.get("owner")!=value.get("fixture",{}).get("owner") or Path(owner.get("evidence","")).resolve()!=output.resolve():
        raise ValueError("native served PostgreSQL fixture ownership changed")
    for name in ("observation",*[row[0] for row in CASES]):
        temporary=root/"cases"/name
        if temporary.is_symlink() or not temporary.is_dir() or any(temporary.iterdir()):
            raise ValueError("native served PostgreSQL temporary state was not removed")
    binaries=validate_bundle(root/"prepared/build.json",value["source"]["digest"])
    if binaries!=value.get("binaries"):raise ValueError("native served PostgreSQL executable receipt changed")
    return value

def worker(args: argparse.Namespace) -> int:
    from rust_test_plan import working_subject
    from _required_build_context import build_context
    digest,facts=working_subject();out=args.output.resolve();out.mkdir(parents=True,exist_ok=False)
    record=dict(schema_version=1,status="running",service="postgres-e2e",platform="Windows/MSVC",full_gate=False,
        linux_verification_allowed=False,public_publication_allowed=False,stages=[],cases=[],
        source=dict(digest=digest,revision=facts["revision"].decode().strip(),index_tree=facts["index_tree"].decode().strip(),
                    patch_sha256=hashlib.sha256(facts["patch"]).hexdigest()),
        compile_context=build_context(ROOT,[]),process_ownership="native Windows deadline Job; owned server EOF shutdown required")
    (out/"source.patch").write_bytes(facts["patch"])
    save_record(out,record) # Initial refusal must occur before creating or launching a fixture.
    root=Path(tempfile.mkdtemp(prefix="awaken-pg-e2e.",dir=args.temp_root.resolve()))
    owner="awaken-ci-"+uuid.uuid4().hex
    (root/"owner.json").write_text(json.dumps(dict(owner=owner,evidence=str(out))),encoding="utf-8")
    record.update(fixture_root=str(root),fixture_data_retained=True)
    service=None;code=0;complete=False
    def save():save_record(out,record)
    def run(name,command,environment=None,directory=ROOT):
        nonlocal code
        log=out/(name+".log");assert not log.exists();started=time.monotonic()
        with log.open("wb") as stream:
            child=subprocess.run(command,cwd=directory,env=environment,stdin=subprocess.DEVNULL,stdout=stream,
                stderr=subprocess.STDOUT,creationflags=getattr(subprocess,"CREATE_NO_WINDOW",0))
            if child.returncode:code=code or child.returncode
        row=dict(stage=name,command=command,cwd=str(directory),exit_code=child.returncode,seconds=time.monotonic()-started,log_sha256=sha256(log))
        record["stages"].append(row);save()
        if child.returncode:raise RuntimeError("native PG E2E child failed at "+name)
        return log.read_text(encoding="utf-8",errors="replace")
    try:
        # This lane first builds complete production/scenario execution hosts.
        # The focused SQL-only fixture threshold cannot admit those builds.
        run("disk-admission",[sys.executable,"-X","utf8","scripts/ci/gate_tools.py","disk","--stage","rust-full"])
        node=windows_executable(args.node_bin)
        service=PostgresService(args.postgres_bin,root/"postgres",out,owner)
        record["node"]=node
        version=run("rustc-identity",["rustc","-vV"])
        if "host: x86_64-pc-windows-msvc" not in version:raise ValueError("MSVC host toolchain is required")
        record["node_version"]=run("node-identity",[node["path"],"--version"]).strip()
        run("node-dependencies",[node["path"],"--input-type=module","-e","await import('@anthropic-ai/sdk');"],directory=ROOT/"e2e")
        text=run("worker-fixture-codec",[node["path"],"--test","--test-reporter=tap",*FIXTURE_TESTS])
        names=validate_worker_fixture_probe(text,0)
        record["worker_fixture_probe"]=dict(status="passed",sources={path:sha256(ROOT/path) for path in FIXTURE_TESTS},cases=names)
        save()
        # Build before PostgreSQL launch; only Cargo's existing artifact/snapshot
        # owner selects these binaries, and no ambient prebuild is accepted.
        prepare=root/"prepared"
        run("build-execution-bundles",[node["path"],"e2e/native_postgres_build.mjs","--output",str(prepare),"--source-digest",digest])
        binaries=validate_bundle(prepare/"build.json",digest);record["binaries"]=binaries
        if working_subject()[0]!=digest:raise ValueError("source changed during native binary preparation")
        record["fixture"]=service.start();save()
        cases_root=root/"cases";cases_root.mkdir()
        observation=cases_root/"observation";observation.mkdir()
        environment=case_environment(dict(os.environ),service.url,observation,binaries,Path(service.binaries["psql"]["path"]))
        text=run("psql-observation",[node["path"],"--test","--test-reporter=tap","e2e/postgres.test.mjs"],environment)
        validate_sql_probe(text,0)
        if list(observation.iterdir()):raise ValueError("native SQL observation left temporary state")
        record["sql_probe"]=dict(status="passed",source="e2e/postgres.test.mjs",source_sha256=sha256(ROOT/"e2e/postgres.test.mjs"));save()
        for case,filename,marker in CASES:
            temporary=cases_root/case;temporary.mkdir()
            # Claim is global to its database. Independent scenarios must not
            # inherit an earlier scenario's pending dispatches or recovery Work.
            # Keep each database for evidence; never truncate prior scenarios.
            database=case_database_name(case)
            service.verify()
            service.sql("CREATE DATABASE "+database)
            url=service.url.rsplit("/",1)[0]+"/"+database
            environment=case_environment(dict(os.environ),url,temporary,binaries,Path(service.binaries["psql"]["path"]))
            command=[node["path"],*(["--experimental-strip-types"] if filename.endswith(".ts") else []),"e2e/"+filename]
            text=run(case,command,environment);validate_case(text,0,marker)
            if list(temporary.iterdir()):raise ValueError("served scenario left owned temporary state after child exit")
            record["cases"].append(dict(case=case,database=database,source="e2e/"+filename,source_sha256=sha256(ROOT/"e2e"/filename),
                marker=marker,status="passed",temporary_removed=True));save()
        validate_bundle(prepare/"build.json",digest)
        service.verify()
        if working_subject()[0]!=digest:raise ValueError("source changed during served PostgreSQL execution")
        record["source_unchanged"]=True;complete=True
    except BaseException as error:
        code=code or (130 if isinstance(error,KeyboardInterrupt) else error.code if isinstance(error,SystemExit) and isinstance(error.code,int) and error.code else 1)
        record.update(error=str(error),error_kind=type(error).__name__)
    finally:
        try:
            if service is not None:
                record["cleanup"]=service.close()
                if record["cleanup"].get("stopped") is not True or record["cleanup"].get("exit_code",0)!=0:
                    raise ValueError("owned PostgreSQL did not shut down cleanly")
        except BaseException as error:
            code=code or 1;record["cleanup_error"]=str(error)
        code=code or int(not complete)
        record.update(status="passed" if code==0 else "failed",exit_code=code)
        try:save()
        except BaseException as error:
            code=code or 1;record.update(status="failed",exit_code=code,recording_error=str(error))
            try:save()
            except BaseException:pass
    print(json.dumps({key:record[key] for key in ("status","service","platform","exit_code")}))
    return code

def main(argv: list[str]) -> int:
    p=argparse.ArgumentParser(allow_abbrev=False)
    p.add_argument("--postgres-bin",type=Path,required=True)
    p.add_argument("--temp-root",type=Path,required=True)
    p.add_argument("--output",type=Path,required=True)
    p.add_argument("--node-bin",type=Path,default=Path(shutil.which("node") or "missing-node.exe"))
    p.add_argument("--timeout",type=int,default=7200)
    p.add_argument("--owned-worker",action="store_true",help=argparse.SUPPRESS)
    args=p.parse_args(argv)
    if os.name!="nt":p.error("native PostgreSQL E2E requires Windows; no container fallback")
    if args.timeout<=0 or not args.temp_root.is_absolute() or not args.temp_root.is_dir() or len(str(args.temp_root.resolve()))>100:
        p.error("positive deadline and existing short absolute task-owned temp root required")
    if args.owned_worker:return worker(args)
    from deadline import run
    code=run(args.timeout,[sys.executable,"-X","utf8",str(ROOT/"scripts/ci/_native_shell.py"),
        "scripts/ci/_provider_environment.sh","--exec",sys.executable,"-X","utf8",str(Path(__file__).resolve()),*argv,"--owned-worker"])
    if code:return code
    reconcile_execution(args.output.resolve())
    return 0

if __name__=="__main__":raise SystemExit(main(sys.argv[1:]))
