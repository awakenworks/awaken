#!/usr/bin/env python3
"""Compare ordinary cold/warm builds of an explicit Rust test workload.

Each observation gets a fresh checkout-local target directory. Test identities
and unchanged warm artifacts are checked; a failed observation is retained and
stops the comparison. This is a benchmark, never a full qualification receipt.
"""
from __future__ import annotations

import argparse
from collections import Counter
import datetime
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import unittest

import crate_test
import nextest_execution
from rust_test_plan import source_snapshot

ROOT = Path(__file__).resolve().parents[2]


def plain_environment(target: Path, jobs: int) -> dict[str, str]:
    environment = dict(os.environ)
    for name in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "AWAKEN_LINK_OUTPUT_STAGING"):
        environment.pop(name, None)
    environment.update(CARGO_TARGET_DIR=str(target), CARGO_BUILD_BUILD_DIR=str(target),
                       CARGO_BUILD_JOBS=str(jobs), CARGO_TERM_COLOR="never", PYTHONUTF8="1")
    return environment


def select_linker(environment: dict[str, str], linker: Path | None) -> dict[str, str]:
    result = dict(environment)
    if linker is not None:
        if result.get('RUSTFLAGS') or result.get('CARGO_ENCODED_RUSTFLAGS'):
            raise ValueError('linker comparisons require an environment without ambient Rust flags')
        result['CARGO_ENCODED_RUSTFLAGS'] = '\x1f'.join(['-Clinker=' + str(linker), '-Clinker-flavor=lld-link'])
    return result


def build_artifacts(text: str) -> tuple[int, dict[str, dict]]:
    compilations = 0
    artifacts = {}
    for line in text.splitlines():
        if not line.startswith("{"):
            continue
        record = json.loads(line)
        if record.get("reason") != "compiler-artifact":
            continue
        compilations += not record["fresh"]
        if record.get("executable") and record.get("profile", {}).get("test"):
            key = record["package_id"] + "::" + record["target"]["name"]
            if key in artifacts:
                raise ValueError("duplicate test artifact: " + key)
            path = Path(record["executable"])
            if not path.is_file() or not path.stat().st_size:
                raise ValueError("missing or empty test executable: " + str(path))
            if os.name == "nt" and (path.stat().st_size < 1024 or path.open("rb").read(2) != b"MZ"):
                raise ValueError("test output is not a native Windows executable: " + str(path))
            artifacts[key] = {"path": str(path), "bytes": path.stat().st_size}
    if not artifacts:
        raise ValueError("the workload produced no test executables")
    return compilations, artifacts


def executed_cases(text: str) -> Counter[tuple[str, str]]:
    cases = Counter(nextest_execution.PASS.findall(text))
    summaries = nextest_execution.SUMMARY.findall(text)
    if len(summaries) != 1:
        raise ValueError("one complete Nextest summary is required")
    run, passed, skipped = (int(value or 0) for value in summaries[0])
    if not run or run != passed or skipped or sum(cases.values()) != run or any(count != 1 for count in cases.values()) or nextest_execution.SKIP.search(text):
        raise ValueError("all nonempty selected tests must execute and pass without skips")
    return cases


def positive(value: str) -> int:
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("a positive integer is required")
    return number


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("-p", "--package", action="append", default=[])
    parser.add_argument("--jobs", nargs="+", type=positive, default=[1, 2, 4])
    parser.add_argument("--rounds", type=positive, default=2)
    parser.add_argument("--timeout", type=positive, default=900)
    parser.add_argument('--linker', choices=('default', 'rust-lld'), default='default')
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return int(not unittest.TextTestRunner().run(
            unittest.defaultTestLoader.loadTestsFromTestCase(BenchmarkTests)).wasSuccessful())
    if not args.package or len(set(args.package)) != len(args.package) or len(set(args.jobs)) != len(args.jobs):
        parser.error("packages and job counts must be nonempty and unique")
    linker = None
    if args.linker == 'rust-lld':
        host = next(line.split(': ', 1)[1] for line in subprocess.check_output(['rustc', '-vV'], text=True).splitlines() if line.startswith('host: '))
        if os.name != 'nt' or not host.endswith('-msvc'):
            parser.error('this rust-lld experiment requires a native Windows MSVC host')
        sysroot = Path(subprocess.check_output(['rustc', '--print', 'sysroot'], text=True).strip())
        linker = sysroot / 'lib/rustlib' / host / 'bin/rust-lld.exe'
        if not linker.is_file():
            parser.error('rust-lld was not found in the active toolchain')
    features, companions = crate_test.load_profile()
    metadata = crate_test.full_graph_metadata(features)
    commands = crate_test.plan_packages(metadata, features, companions, args.package, [])
    test = commands[-1]
    build = ["cargo", "test", *test[3:], "--no-run", "--timings", "--message-format=json"]
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    output = ROOT / "target/test-timings" / ("build-benchmark-" + stamp)
    output.mkdir(parents=True)
    digest, patch = source_snapshot()
    (output / "source.patch").write_bytes(patch)
    receipt = {"kind": "ordinary-rust-build-benchmark", "full_gate": False, "status": "running",
               "source_digest": digest, "source_head": subprocess.check_output(
                   ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
               "rustc": subprocess.check_output(["rustc", "-vV"], cwd=ROOT, text=True).strip(),
               "packages": args.package, "jobs": args.jobs, "rounds": args.rounds,
               "compiler_wrapper": None, "compiler_output_staging": False,
               "linker": str(linker) if linker else 'toolchain default',
               "cache": "fresh target/build directory for every observation", "rows": []}
    def save() -> None:
        (output / "results.json").write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    print("Benchmark evidence: " + str(output), flush=True)
    save()
    status = 1
    expected_cases = None
    expected_artifacts = None
    try:
        for round_index in range(args.rounds):
            # Alternate order so the first job count does not always see the
            # coldest filesystem/compiler caches. Cargo artifact caches stay isolated.
            order = args.jobs if round_index % 2 == 0 else list(reversed(args.jobs))
            for jobs in order:
                target = output / f"round-{round_index + 1}-jobs-{jobs}"
                environment = select_linker(plain_environment(target, jobs), linker)
                for index, command in enumerate([*commands[:-1], build, test, build]):
                    phase = "companion" if index < len(commands) - 1 else (
                        "cold-build", "tests", "warm-build")[index - len(commands) + 1]
                    log = output / f"round-{round_index + 1}-jobs-{jobs}-{index}-{phase}.log"
                    print(f"START round={round_index + 1} jobs={jobs} {phase}", flush=True)
                    started = time.monotonic()
                    stages = None
                    if phase == 'tests':
                        test_status, stages = nextest_execution.execute(command, environment,
                            output / f'round-{round_index + 1}-jobs-{jobs}-execution', args.timeout)
                        result = subprocess.CompletedProcess(command, test_status)
                        log.write_text('\n'.join(Path(stage[path]).read_text(encoding='utf-8')
                            for stage in stages for path in ('log', 'stderr') if Path(stage[path]).is_file()), encoding='utf-8')
                    else:
                        with log.open("w", encoding="utf-8") as stream:
                            result = subprocess.run([sys.executable, str(ROOT / "scripts/ci/deadline.py"),
                                                     str(args.timeout), "--", *command],
                                                    cwd=ROOT, env=environment, stdout=stream, stderr=subprocess.STDOUT)
                    row = {"round": round_index + 1, "jobs": jobs, "phase": phase,
                           "command": command, "log": str(log), "exit_code": result.returncode,
                           "elapsed_seconds": time.monotonic() - started}
                    receipt["rows"].append(row)
                    if stages is not None:
                        row['stages'] = stages
                    save()
                    if result.returncode:
                        status = result.returncode
                        raise ValueError("failed observation; comparison stopped, see " + str(log))
                    text = log.read_text(encoding="utf-8")
                    if phase in ("cold-build", "warm-build"):
                        count, artifacts = build_artifacts(text)
                        row.update(compilations=count, test_artifacts=artifacts)
                        row['test_executable_bytes'] = sum(artifact['bytes'] for artifact in artifacts.values())
                        names = sorted(artifacts)
                        if expected_artifacts is not None and names != expected_artifacts:
                            raise ValueError("job counts changed the test artifact identities")
                        expected_artifacts = names
                        if phase == "warm-build" and count:
                            raise ValueError("unchanged warm build unexpectedly recompiled")
                    elif phase == "tests":
                        cases = executed_cases(text)
                        if expected_cases is not None and cases != expected_cases:
                            raise ValueError("job counts changed the executed test identities")
                        expected_cases = cases
                        row["executed_cases"] = sorted(cases.elements())
                    save()
                    print(f"PASS {phase}: {row['elapsed_seconds']:.3f}s", flush=True)
        status = 0
    except (OSError, ValueError) as error:
        receipt["error"] = str(error)
        print(str(error), file=sys.stderr)
    finally:
        receipt["source_unchanged"] = source_snapshot()[0] == digest
        if not receipt["source_unchanged"]:
            status = status or 1
            receipt["error"] = "source changed during benchmark"
        receipt.update(status="passed" if status == 0 else "failed", exit_code=status)
        save()
    return status


class BenchmarkTests(unittest.TestCase):
    def test_explicit_linker_preserves_spaced_path_and_refuses_ambient_flags(self):
        linker = Path('toolchain with spaces/rust-lld.exe')
        result = select_linker({'CARGO_BUILD_JOBS': '1'}, linker)
        self.assertEqual(result['CARGO_ENCODED_RUSTFLAGS'].split('\x1f'), ['-Clinker=' + str(linker), '-Clinker-flavor=lld-link'])
        self.assertEqual(select_linker({'RUSTFLAGS': 'custom'}, None), {'RUSTFLAGS': 'custom'})
        for name in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS'):
            with self.subTest(name=name), self.assertRaises(ValueError): select_linker({name: 'custom'}, linker)

    def test_execution_evidence_rejects_empty_skipped_missing_and_incomplete_runs(self) -> None:
        good = " PASS [ 0.1s] (1/1) package target::case\n Summary [ 0.1s] 1 tests run: 1 passed, 0 skipped\n"
        self.assertEqual(executed_cases(good), Counter({("package", "target::case"): 1}))
        self.assertEqual(executed_cases(good.replace(", 0 skipped", "")), Counter({("package", "target::case"): 1}))
        self.assertEqual(executed_cases(good.replace('(1/1) ', '').replace('1 tests run', '1 test run')),
                         Counter({("package", "target::case"): 1}))
        for invalid in ("", good.replace("0 skipped", "1 skipped"),
                        good.replace("1 tests run", "0 tests run"), good.splitlines()[-1], good + good):
            with self.assertRaises(ValueError):
                executed_cases(invalid)

    def test_ordinary_environment_uses_exact_owned_target_and_job_count(self) -> None:
        target = ROOT / "target/test-timings/probe"
        result = plain_environment(target, 2)
        self.assertEqual(result["CARGO_TARGET_DIR"], str(target))
        self.assertEqual(result["CARGO_BUILD_BUILD_DIR"], str(target))
        self.assertEqual(result["CARGO_BUILD_JOBS"], "2")
        for name in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "AWAKEN_LINK_OUTPUT_STAGING"):
            self.assertNotIn(name, result)


if __name__ == "__main__":
    raise SystemExit(main())
