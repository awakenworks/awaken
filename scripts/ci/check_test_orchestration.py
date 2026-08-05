#!/usr/bin/env python3
"""Keep the repository's test graph on one executable authority.

``e2e/package.json`` owns suite composition and
``e2e/stage_change_coverage_e2e.ts`` owns functional obligations.  CI and
coverage scripts may invoke those authorities, but may not maintain their own
scenario lists or turn a failed test into a successful report.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PACKAGE = ROOT / "e2e/package.json"
STAGE_GRAPH = ROOT / "e2e/stage_change_coverage_e2e.ts"

REQUIRED_DETERMINISTIC_SUITES = (
    "test",
    "test:protocols",
    "test:management",
    "test:durable",
    "test:fs",
    "test:extended",
    "test:environment-matrix",
    "test:coordinator-authority",
    "test:runtime-stages",
    "test:coverage-gaps",
)
SECONDARY_RUNNERS = (
    "scripts/ci/e2e-coverage.sh",
    "scripts/ci/combined-coverage.sh",
)
REQUIRED_RELEASE_COMMANDS = (
    "check_test_orchestration.py",
    "check_public_api.sh --require-tools",
    "cargo deny --log-level error check bans",
    "check_formal.sh --require-tools",
    "pg_tests.sh --require-docker",
    "scripts/e2e/k8s_container_e2e.sh",
    "AWAKEN_K3D_REQUIRED=1 e2e/k3d/distributed_control_e2e.sh",
    "AWAKEN_K3D_REQUIRED=1 e2e/k3d/nats_wake_e2e.sh 12",
    "npm --prefix e2e run test:deterministic",
    "sandbox_capability_suite.sh --require-substrates",
)
STORE_CONFORMANCE = "crates/stores/awaken-store-conformance/src/lib.rs"
STORE_BACKEND_TESTS = (
    "crates/runtime/awaken-store-inmem/tests/conformance.rs",
    "crates/stores/awaken-store-fs/tests/conformance.rs",
    "crates/stores/awaken-store-sqlite/tests/checkpoint_reader.rs",
    "crates/stores/awaken-store-postgres/tests/postgres_live.rs",
)
INGRESS_CONFORMANCE = "crates/server/awaken-run-ingress/tests/dispatch_conformance.rs"


def e2e_files(root: Path) -> set[str]:
    return {
        path.relative_to(root / "e2e").as_posix()
        for path in (root / "e2e").rglob("*")
        if path.is_file()
        and "e2e" in path.stem
        and path.suffix in {".js", ".mjs", ".ts"}
    }


def orchestration_errors(
    scripts: dict[str, str],
    stage_text: str,
    files: set[str],
    runner_texts: dict[str, str],
) -> list[str]:
    errors: list[str] = []
    deterministic = scripts.get("test:deterministic", "")

    for suite in REQUIRED_DETERMINISTIC_SUITES:
        if f"npm run {suite}" not in deterministic:
            errors.append(f"test:deterministic does not invoke {suite}")

    scenario_text, _, obligation_text = stage_text.partition("const obligations")
    scenario_ids = re.findall(r"\bid:\s*'([^']+)'", scenario_text)
    scenario_files = re.findall(r"\bfile:\s*'e2e/([^']+)'", scenario_text)
    obligation_ids = re.findall(r"\bid:\s*'([^']+)'", obligation_text)
    obligation_scenarios = re.findall(r"\bscenario:\s*'([^']+)'", obligation_text)
    if len(scenario_ids) != len(set(scenario_ids)):
        errors.append("stage cause/effect graph contains duplicate scenario ids")
    missing_scenarios = sorted(set(obligation_scenarios) - set(scenario_ids))
    if missing_scenarios:
        errors.append(
            "stage obligations reference unknown scenarios: " + ", ".join(missing_scenarios)
        )
    duplicate_obligations = sorted(
        obligation_id
        for obligation_id in set(obligation_ids)
        if obligation_ids.count(obligation_id) > 1
    )
    if duplicate_obligations:
        errors.append(
            "stage cause/effect graph contains duplicate obligation ids: "
            + ", ".join(duplicate_obligations)
        )
    for relative in scenario_files:
        if relative not in files:
            errors.append(f"stage scenario file does not exist: {relative}")

    # The package scripts plus the executable stage cause/effect graph are the
    # complete classification source. A newly added E2E must be deterministic,
    # live/opt-in, or a stage scenario; an unclassified file is never silently
    # omitted from every gate.
    classified_text = "\n".join(scripts.values()) + "\n" + stage_text
    unclassified = sorted(
        relative
        for relative in files
        if relative not in classified_text and Path(relative).name not in classified_text
    )
    if unclassified:
        errors.append("unclassified E2E files: " + ", ".join(unclassified))

    for relative, text in runner_texts.items():
        if "run test:deterministic" not in text:
            errors.append(f"{relative} does not delegate to test:deterministic")
        if re.search(r"\|\|\s*true", text):
            errors.append(f"{relative} suppresses command failure with `|| true`")
        if "*_e2e" in text:
            errors.append(f"{relative} maintains a parallel E2E filename loop")
    return errors


def release_gate_errors(check_all: str, public_api: str) -> list[str]:
    errors = [
        f"check-all does not require: {command}"
        for command in REQUIRED_RELEASE_COMMANDS
        if command not in check_all
    ]
    if re.search(r'^excluded="[^"\n]+"', public_api, re.MULTILINE):
        errors.append("public API gate still excludes workspace crates")
    return errors


def shared_conformance_errors(
    store_suite: str,
    backend_tests: dict[str, str],
    ingress_tests: str,
) -> list[str]:
    errors: list[str] = []
    store_cases = re.findall(r"^pub async fn ([a-z0-9_]+)", store_suite, re.MULTILINE)
    for relative, test_text in backend_tests.items():
        missing = [
            case
            for case in store_cases
            if f"awaken_store_conformance::{case}" not in test_text
        ]
        if missing:
            errors.append(
                f"{relative} omits shared store cases: " + ", ".join(missing)
            )

    # The ingress testkit intentionally owns no backend. Its executable proof is
    # the same suite applied to each production implementation.
    for backend in ("memory", "sqlite", "postgres"):
        if f"async fn {backend}_dispatch_conforms" not in ingress_tests:
            errors.append(f"dispatch conformance omits {backend} backend")
    if ingress_tests.count("assert_dispatch_conformance(") != 3:
        errors.append("dispatch conformance suite is not applied to exactly three backends")
    if ingress_tests.count("assert_dispatch_operational_feed_conformance(") != 3:
        errors.append("dispatch operational-feed suite is not applied to exactly three backends")
    return errors


def validate(root: Path) -> list[str]:
    package = json.loads((root / "e2e/package.json").read_text(encoding="utf-8"))
    scripts: dict[str, str] = package.get("scripts", {})
    stage_text = (root / "e2e/stage_change_coverage_e2e.ts").read_text(encoding="utf-8")
    errors = orchestration_errors(
        scripts,
        stage_text,
        e2e_files(root),
        {
            relative: (root / relative).read_text(encoding="utf-8")
            for relative in SECONDARY_RUNNERS
        },
    )
    errors.extend(
        release_gate_errors(
            (root / "scripts/ci/check-all.sh").read_text(encoding="utf-8"),
            (root / "scripts/ci/check_public_api.sh").read_text(encoding="utf-8"),
        )
    )
    errors.extend(
        shared_conformance_errors(
            (root / STORE_CONFORMANCE).read_text(encoding="utf-8"),
            {
                relative: (root / relative).read_text(encoding="utf-8")
                for relative in STORE_BACKEND_TESTS
            },
            (root / INGRESS_CONFORMANCE).read_text(encoding="utf-8"),
        )
    )
    return errors


def self_test() -> None:
    # Cause/effect decision table for the checker itself:
    # C1 canonical aggregate includes every required suite;
    # C2 every E2E belongs to package scripts or the stage cause/effect graph;
    # C3 secondary runners delegate to the aggregate; C4 secondary runners
    # preserve failure status; C5 functional obligation ids are unique; C6 the
    # release gate requires every external/API/dependency suite without exclusions;
    # C7 every shared conformance testkit is executed by every production backend.
    #
    # | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
    # | R1   | T  | T  | T  | T  | T  | T  | accept |
    # | R2   | F  | *  | *  | *  | *  | *  | reject missing suite |
    # | R3   | T  | F  | *  | *  | *  | *  | reject unclassified E2E |
    # | R4   | T  | T  | F  | *  | *  | *  | reject parallel runner |
    # | R5   | T  | T  | T  | F  | *  | *  | reject swallowed failure |
    # | R6   | T  | T  | T  | T  | F  | *  | reject duplicate obligation |
    # | R7   | T  | T  | T  | T  | T  | F  | reject incomplete release gate |
    # | R8   | T  | T  | T  | T  | T  | T, C7=F | reject detached testkit/backend |
    package = json.loads(PACKAGE.read_text(encoding="utf-8"))
    scripts: dict[str, str] = package["scripts"]
    stage_text = STAGE_GRAPH.read_text(encoding="utf-8")
    files = e2e_files(ROOT)
    runners = {
        relative: (ROOT / relative).read_text(encoding="utf-8")
        for relative in SECONDARY_RUNNERS
    }
    errors = orchestration_errors(scripts, stage_text, files, runners)
    if errors:
        raise AssertionError("R1 repository fixture must be valid: " + "; ".join(errors))

    missing_suite = dict(scripts)
    missing_suite["test:deterministic"] = missing_suite["test:deterministic"].replace(
        "npm run test:protocols", ""
    )
    assert any(
        "does not invoke test:protocols" in error
        for error in orchestration_errors(missing_suite, stage_text, files, runners)
    ), "R2"

    unclassified = set(files)
    unclassified.add("unclassified_e2e.mjs")
    assert any(
        "unclassified_e2e.mjs" in error
        for error in orchestration_errors(scripts, stage_text, unclassified, runners)
    ), "R3"

    parallel = dict(runners)
    parallel[SECONDARY_RUNNERS[0]] = "for f in *_e2e.mjs; do node $f; done"
    parallel_errors = orchestration_errors(scripts, stage_text, files, parallel)
    assert any("does not delegate" in error for error in parallel_errors), "R4 delegate"
    assert any("parallel E2E" in error for error in parallel_errors), "R4 list"

    swallowed = dict(runners)
    swallowed[SECONDARY_RUNNERS[1]] += "\nnpm run test:deterministic || true\n"
    assert any(
        "suppresses command failure" in error
        for error in orchestration_errors(scripts, stage_text, files, swallowed)
    ), "R5"

    duplicate_obligation = stage_text.replace(
        "{ id: 'D0-02'", "{ id: 'D0-01'", 1
    )
    assert any(
        "duplicate obligation ids" in error
        for error in orchestration_errors(
            scripts, duplicate_obligation, files, runners
        )
    ), "R6"

    check_all = (ROOT / "scripts/ci/check-all.sh").read_text(encoding="utf-8")
    public_api = (ROOT / "scripts/ci/check_public_api.sh").read_text(encoding="utf-8")
    assert not release_gate_errors(check_all, public_api), "R1/C6"
    assert release_gate_errors(
        check_all.replace("pg_tests.sh --require-docker", "pg_tests.sh"),
        public_api,
    ), "R7 required infrastructure"
    assert release_gate_errors(
        check_all.replace("scripts/e2e/k8s_container_e2e.sh", ""),
        public_api,
    ), "R7 required Kubernetes infrastructure"
    assert release_gate_errors(check_all, public_api + '\nexcluded="awaken-cli"\n'), (
        "R7 public API exclusion"
    )

    store_suite = (ROOT / STORE_CONFORMANCE).read_text(encoding="utf-8")
    backend_tests = {
        relative: (ROOT / relative).read_text(encoding="utf-8")
        for relative in STORE_BACKEND_TESTS
    }
    ingress_tests = (ROOT / INGRESS_CONFORMANCE).read_text(encoding="utf-8")
    assert not shared_conformance_errors(store_suite, backend_tests, ingress_tests), "R1/C7"
    detached_backend = dict(backend_tests)
    detached_backend[STORE_BACKEND_TESTS[0]] = detached_backend[
        STORE_BACKEND_TESTS[0]
    ].replace("awaken_store_conformance::commit_then_read", "detached::commit_then_read")
    assert any(
        "omits shared store cases: commit_then_read" in error
        for error in shared_conformance_errors(
            store_suite, detached_backend, ingress_tests
        )
    ), "R8 store testkit"
    assert any(
        "omits postgres backend" in error
        for error in shared_conformance_errors(
            store_suite,
            backend_tests,
            ingress_tests.replace("async fn postgres_dispatch_conforms", "async fn detached"),
        )
    ), "R8 ingress testkit"


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        self_test()
        return 0
    if argv:
        print("usage: check_test_orchestration.py [--self-test]", file=sys.stderr)
        return 2
    errors = validate(ROOT)
    if errors:
        print("check-test-orchestration:", file=sys.stderr)
        for error in errors:
            print(f"  {error}", file=sys.stderr)
        return 1
    print("check-test-orchestration: canonical suite graph is complete")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
