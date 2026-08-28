#!/usr/bin/env python3
"""Keep the repository's test graph on one executable authority.

``e2e/package.json`` owns suite composition and its deterministic suite order;
``e2e/stage_change_coverage_e2e.ts`` owns functional obligations.  CI and
coverage scripts may invoke those authorities, but may not maintain their own
scenario lists, execute a scenario twice, or turn a failure into success.
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
    "test:composition",
    "test:coordinator-authority",
    "test:runtime-stages",
    "test:coverage-gaps",
    "test:delegation-restart",
    "test:compatibility",
)
DETERMINISTIC_RUNNER = (
    "../scripts/ci/_provider_environment.sh --exec node deterministic_runner.mjs"
)
SECONDARY_RUNNERS = (
    "scripts/ci/e2e-coverage.sh",
    "scripts/ci/combined-coverage.sh",
)
PARALLEL_COVERAGE_MARKER = re.compile(
    r"(?:\bcargo\s+llvm-cov\b|\bllvm-cov\s+show-env\b|\*_e2e)"
)
REQUIRED_RELEASE_COMMANDS = (
    "check_test_orchestration.py",
    "npm --prefix e2e run test:runner",
    "check_public_api.sh --require-tools",
    "cargo deny --log-level error check bans",
    "check_formal.sh --require-tools",
    "pg_tests.sh --require-docker",
    "scripts/e2e/k8s_container_e2e.sh",
    "AWAKEN_K3D_REQUIRED=1 e2e/k3d/distributed_control_e2e.sh",
    "AWAKEN_K3D_REQUIRED=1 e2e/k3d/nats_wake_e2e.sh 12",
    "npm --prefix e2e run test:deterministic",
    "npm --prefix e2e run test:sdk-behavior-owners",
    "npm --prefix e2e run test:sdk-latest-canary",
    "sandbox_capability_suite.sh --require-substrates",
)
REQUIRED_RELEASE_GROUPS = (
    "static",
    "docs",
    "rust",
    "api",
    "formal",
    "postgres",
    "kubernetes",
    "k3d",
    "frontend",
    "e2e",
    "sandbox",
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


def coverage_runner_texts(root: Path) -> dict[str, str]:
    """Load the canonical runners plus any competing E2E-local coverage shell."""

    runners = {
        relative: (root / relative).read_text(encoding="utf-8")
        for relative in SECONDARY_RUNNERS
    }
    for path in (root / "e2e").rglob("*.sh"):
        text = path.read_text(encoding="utf-8")
        if PARALLEL_COVERAGE_MARKER.search(text):
            runners[path.relative_to(root).as_posix()] = text
    return runners


def orchestration_errors(
    scripts: dict[str, str],
    deterministic_suites: list[str],
    stage_text: str,
    files: set[str],
    runner_texts: dict[str, str],
) -> list[str]:
    errors: list[str] = []
    if scripts.get("test:deterministic") != DETERMINISTIC_RUNNER:
        errors.append(
            "test:deterministic does not enter the canonical provider environment "
            "sanitizer before deterministic_runner.mjs"
        )
    if len(deterministic_suites) != len(set(deterministic_suites)):
        errors.append("deterministic suite order contains duplicates")

    for suite in REQUIRED_DETERMINISTIC_SUITES:
        if suite not in deterministic_suites:
            errors.append(f"deterministic suite order does not invoke {suite}")

    expanded: list[tuple[str, str]] = []

    def visit(name: str, stack: tuple[str, ...] = ()) -> None:
        if name in stack:
            errors.append("cyclic deterministic suite: " + " -> ".join((*stack, name)))
            return
        body = scripts.get(name)
        if body is None:
            errors.append(f"deterministic suite does not exist: {name}")
            return
        if name == "test" and scripts.get("pretest"):
            visit("pretest", (*stack, name))
        for command in re.split(r"\s*&&\s*", body):
            nested = re.fullmatch(r"npm run ([^ ]+)", command)
            if nested and nested.group(1) in scripts:
                visit(nested.group(1), (*stack, name))
            else:
                expanded.append((command, " > ".join((*stack, name))))

    for suite in deterministic_suites:
        visit(suite)

    by_command: dict[str, list[str]] = {}
    for command, owner in expanded:
        by_command.setdefault(command, []).append(owner)
    for command, owners in sorted(by_command.items()):
        if len(owners) > 1:
            errors.append(
                f"duplicate deterministic command `{command}` via " + ", ".join(owners)
            )

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

    direct_scenario_files: set[str] = set()
    for command, _ in expanded:
        direct_scenario_files.update(
            re.findall(r"(?:^|\s)(?:node|tsx)\s+([^\s]+_e2e\.(?:js|mjs|ts))", command)
        )
    stage_overlap = sorted(set(scenario_files) & direct_scenario_files)
    if stage_overlap:
        errors.append(
            "stage scenarios also execute in deterministic package suites: "
            + ", ".join(stage_overlap)
        )

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
        if relative not in SECONDARY_RUNNERS:
            errors.append(
                f"{relative} is a parallel E2E coverage runner; "
                "use the canonical scripts/ci runners"
            )
            continue
        if "run test:deterministic" not in text:
            errors.append(f"{relative} does not delegate to test:deterministic")
        if not re.search(
            r"^source scripts/ci/_provider_environment\.sh$", text, re.MULTILINE
        ) or not re.search(r"^awaken_unset_ambient_api_keys$", text, re.MULTILINE):
            errors.append(
                f"{relative} does not use the canonical provider environment sanitizer"
            )
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
    for group in REQUIRED_RELEASE_GROUPS:
        if not re.search(rf"^run {re.escape(group)} ", check_all, re.MULTILINE):
            errors.append(f"check-all has no executable {group} group")
    if "duration_seconds" not in check_all or "AWAKEN_TEST_TIMINGS_FILE" not in check_all:
        errors.append("check-all does not publish a configurable timing artifact")
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
    # Cause C7: a backend may inject its store-owned clock through the canonical
    # conformance wrapper. Effect: exactly three backend bindings still execute
    # one shared suite; neither helper spelling creates a parallel case list.
    conformance_bindings = re.findall(
        r"\bassert_dispatch_conformance(?:_with_clock)?\(", ingress_tests
    )
    if len(conformance_bindings) != 3:
        errors.append("dispatch conformance suite is not applied to exactly three backends")
    operational_feed_bindings = re.findall(
        r"\bassert_dispatch_operational_feed_conformance(?:_with_clock)?\(",
        ingress_tests,
    )
    if len(operational_feed_bindings) != 3:
        errors.append("dispatch operational-feed suite is not applied to exactly three backends")
    return errors


def validate(root: Path) -> list[str]:
    package = json.loads((root / "e2e/package.json").read_text(encoding="utf-8"))
    scripts: dict[str, str] = package.get("scripts", {})
    deterministic_suites = package.get("awakenTest", {}).get("deterministicSuites", [])
    if not isinstance(deterministic_suites, list) or not all(
        isinstance(suite, str) for suite in deterministic_suites
    ):
        return ["awakenTest.deterministicSuites must be an array of script names"]
    stage_text = (root / "e2e/stage_change_coverage_e2e.ts").read_text(encoding="utf-8")
    errors = orchestration_errors(
        scripts,
        deterministic_suites,
        stage_text,
        e2e_files(root),
        coverage_runner_texts(root),
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
    # C7 every shared conformance testkit is executed by every production backend;
    # C8 deterministic leaf commands are unique; C9 stage scenarios have one owner.
    # `test:compatibility` is part of C1, while the network-backed latest-SDK
    # canary is part of C6 so it runs once at the release boundary rather than
    # contaminating the hermetic deterministic runner.
    #
    # | Rule | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | C9 | Effect |
    # | R1   | T  | T  | T  | T  | T  | T  | T  | T  | T  | accept |
    # | R2   | F  | *  | *  | *  | *  | *  | *  | *  | *  | reject missing suite |
    # | R3   | T  | F  | *  | *  | *  | *  | *  | *  | *  | reject unclassified E2E |
    # | R4   | T  | T  | F  | *  | *  | *  | *  | *  | *  | reject parallel runner |
    # | R5   | T  | T  | T  | F  | *  | *  | *  | *  | *  | reject swallowed failure |
    # | R6   | T  | T  | T  | T  | F  | *  | *  | *  | *  | reject duplicate obligation |
    # | R7   | T  | T  | T  | T  | T  | F  | *  | *  | *  | reject incomplete release gate |
    # | R8   | T  | T  | T  | T  | T  | T  | F  | *  | *  | reject detached testkit/backend |
    # | R9   | T  | T  | T  | T  | T  | T  | T  | F  | *  | reject duplicate command |
    # | R10  | T  | T  | T  | T  | T  | T  | T  | T  | F  | reject package/stage overlap |
    package = json.loads(PACKAGE.read_text(encoding="utf-8"))
    scripts: dict[str, str] = package["scripts"]
    deterministic_suites: list[str] = package["awakenTest"]["deterministicSuites"]
    stage_text = STAGE_GRAPH.read_text(encoding="utf-8")
    files = e2e_files(ROOT)
    runners = coverage_runner_texts(ROOT)
    errors = orchestration_errors(scripts, deterministic_suites, stage_text, files, runners)
    if errors:
        raise AssertionError("R1 repository fixture must be valid: " + "; ".join(errors))

    # Deterministic-environment rule: C10=the package entry crosses the one
    # provider-environment sanitizer before Node starts; E10=every expanded leaf
    # receives one already-sanitized environment snapshot. R11 C10=>E10;
    # !C10 rejects a raw runner entry that would force JavaScript callers to own
    # another provider-key predicate.
    raw_deterministic_entry = dict(scripts)
    raw_deterministic_entry["test:deterministic"] = "node deterministic_runner.mjs"
    assert any(
        "does not enter the canonical provider environment sanitizer" in error
        for error in orchestration_errors(
            raw_deterministic_entry,
            deterministic_suites,
            stage_text,
            files,
            runners,
        )
    ), "R11 canonical deterministic environment"

    missing_suite = [suite for suite in deterministic_suites if suite != "test:protocols"]
    assert any(
        "does not invoke test:protocols" in error
        for error in orchestration_errors(scripts, missing_suite, stage_text, files, runners)
    ), "R2"
    missing_compatibility = [
        suite for suite in deterministic_suites if suite != "test:compatibility"
    ]
    assert any(
        "does not invoke test:compatibility" in error
        for error in orchestration_errors(
            scripts, missing_compatibility, stage_text, files, runners
        )
    ), "R2 compatibility"

    unclassified = set(files)
    unclassified.add("unclassified_e2e.mjs")
    assert any(
        "unclassified_e2e.mjs" in error
        for error in orchestration_errors(scripts, deterministic_suites, stage_text, unclassified, runners)
    ), "R3"

    parallel = dict(runners)
    parallel[SECONDARY_RUNNERS[0]] = "for f in *_e2e.mjs; do node $f; done"
    parallel_errors = orchestration_errors(scripts, deterministic_suites, stage_text, files, parallel)
    assert any("does not delegate" in error for error in parallel_errors), "R4 delegate"
    assert any("parallel E2E" in error for error in parallel_errors), "R4 list"

    # Coverage-runner cause/effect design: C11=an E2E-local shell runs llvm-cov
    # or owns a scenario glob; C12=a canonical coverage runner omits the shared
    # provider-key sanitizer. Effects: E11=reject a second suite authority;
    # E12=reject ambient-secret-dependent deterministic execution. Constraint:
    # only the two SECONDARY_RUNNERS may orchestrate coverage, both delegate to
    # test:deterministic, and both source/call the single canonical sanitizer.
    #
    # | Rule | C11 | C12 | Effect |
    # | R12  | T   | -   | E11: reject the competing runner |
    # | R13  | F   | T   | E12: reject the unsanitized canonical runner |
    # | R1   | F   | F   | apply the existing C1-C10 acceptance rules |
    reintroduced_runner = dict(runners)
    reintroduced_runner["e2e/coverage.sh"] = (
        "eval \"$(cargo llvm-cov show-env --sh)\"\n"
        "for f in *_e2e.mjs; do node \"$f\"; done\n"
    )
    assert any(
        "e2e/coverage.sh is a parallel E2E coverage runner" in error
        for error in orchestration_errors(
            scripts,
            deterministic_suites,
            stage_text,
            files,
            reintroduced_runner,
        )
    ), "R12 parallel coverage runner"

    unsanitized = dict(runners)
    unsanitized[SECONDARY_RUNNERS[0]] = unsanitized[SECONDARY_RUNNERS[0]].replace(
        "source scripts/ci/_provider_environment.sh\nawaken_unset_ambient_api_keys\n",
        "",
        1,
    )
    assert any(
        "does not use the canonical provider environment sanitizer" in error
        for error in orchestration_errors(
            scripts,
            deterministic_suites,
            stage_text,
            files,
            unsanitized,
        )
    ), "R13 canonical provider sanitizer"

    swallowed = dict(runners)
    swallowed[SECONDARY_RUNNERS[1]] += "\nnpm run test:deterministic || true\n"
    assert any(
        "suppresses command failure" in error
        for error in orchestration_errors(scripts, deterministic_suites, stage_text, files, swallowed)
    ), "R5"

    duplicate_obligation = stage_text.replace(
        "{ id: 'D0-02'", "{ id: 'D0-01'", 1
    )
    assert any(
        "duplicate obligation ids" in error
        for error in orchestration_errors(
            scripts, deterministic_suites, duplicate_obligation, files, runners
        )
    ), "R6"

    duplicate_command = dict(scripts)
    duplicate_command["test:protocols"] += " && node managed_e2e.mjs"
    assert any(
        "duplicate deterministic command `node managed_e2e.mjs`" in error
        for error in orchestration_errors(
            duplicate_command, deterministic_suites, stage_text, files, runners
        )
    ), "R9"

    stage_overlap = dict(scripts)
    stage_overlap["test:protocols"] += " && node sandbox_provisioning_e2e.mjs"
    assert any(
        "stage scenarios also execute" in error
        for error in orchestration_errors(
            stage_overlap, deterministic_suites, stage_text, files, runners
        )
    ), "R10"

    check_all = (ROOT / "scripts/ci/check-all.sh").read_text(encoding="utf-8")
    public_api = (ROOT / "scripts/ci/check_public_api.sh").read_text(encoding="utf-8")
    assert not release_gate_errors(check_all, public_api), "R1/C6"
    assert release_gate_errors(
        check_all.replace("pg_tests.sh --require-docker", "pg_tests.sh"),
        public_api,
    ), "R7 required infrastructure"
    assert release_gate_errors(
        check_all.replace("npm --prefix e2e run test:sdk-behavior-owners", ""),
        public_api,
    ), "R7 supported SDK behavior compatibility"
    assert release_gate_errors(
        check_all.replace("npm --prefix e2e run test:sdk-latest-canary", ""),
        public_api,
    ), "R7 latest SDK compatibility"
    assert release_gate_errors(
        check_all.replace("scripts/e2e/k8s_container_e2e.sh", ""),
        public_api,
    ), "R7 required Kubernetes infrastructure"
    assert release_gate_errors(check_all, public_api + '\nexcluded="awaken-cli"\n'), (
        "R7 public API exclusion"
    )
    without_e2e_group = check_all.replace(
        'run e2e "deterministic-e2e"', 'run static "deterministic-e2e"'
    ).replace(
        'run e2e "managed-sdk-anchor-behavior"',
        'run static "managed-sdk-anchor-behavior"',
    ).replace(
        'run e2e "latest-managed-sdk"', 'run static "latest-managed-sdk"'
    )
    assert release_gate_errors(
        without_e2e_group,
        public_api,
    ), "R7 independently sharded release group"
    assert release_gate_errors(
        check_all.replace("AWAKEN_TEST_TIMINGS_FILE", "REMOVED_TIMINGS_FILE"),
        public_api,
    ), "R7 timing artifact"

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

    # Operational-feed binding cause/effect design:
    # C13=a production backend uses the canonical default-clock entrypoint;
    # C14=a production backend uses the canonical explicit-clock entrypoint.
    # E13=both spellings count as one binding to the same shared suite. R14
    # rejects a missing binding; R15 rejects a duplicate binding, regardless of
    # which spelling introduced it. This keeps clock policy orthogonal to suite
    # ownership and preserves the exactly-three-backends invariant.
    assert not shared_conformance_errors(
        store_suite, backend_tests, ingress_tests
    ), "R1/C13-C14 mixed operational-feed clocks"
    missing_operational_feed = ingress_tests.replace(
        "assert_dispatch_operational_feed_conformance(&store, \"conformance-postgres\").await;",
        "",
        1,
    )
    assert any(
        "operational-feed suite is not applied to exactly three backends" in error
        for error in shared_conformance_errors(
            store_suite, backend_tests, missing_operational_feed
        )
    ), "R14 missing operational-feed binding"
    duplicate_operational_feed = ingress_tests.replace(
        "assert_dispatch_operational_feed_conformance(&store, \"conformance-postgres\").await;",
        (
            "assert_dispatch_operational_feed_conformance(&store, \"conformance-postgres\").await;\n"
            "    assert_dispatch_operational_feed_conformance_with_clock(\n"
            "        &store, \"conformance-postgres-duplicate\", &set_clock,\n"
            "    ).await;"
        ),
        1,
    )
    assert any(
        "operational-feed suite is not applied to exactly three backends" in error
        for error in shared_conformance_errors(
            store_suite, backend_tests, duplicate_operational_feed
        )
    ), "R15 duplicate operational-feed binding"


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
