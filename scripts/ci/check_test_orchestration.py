#!/usr/bin/env python3
"""Keep the repository's test graph on one executable authority.

``e2e/package.json`` owns suite composition and its deterministic suite order;
``e2e/stage_change_coverage_e2e.ts`` owns functional obligations.  CI and
coverage scripts may invoke those authorities, but may not maintain their own
scenario lists, execute a scenario twice, or turn a failure into success.
"""

from __future__ import annotations

import json
import os
import re
import sys
import tempfile
import tomllib
from collections.abc import Iterator
from pathlib import Path

import generate_hooks

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
    "test:acp-tool-bridge",
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
REQUIRED_STAGE_SCENARIOS = (
    ("managed_full_chain", "managed_full_chain_e2e.mjs"),
    ("managed_mcp_hot_swap", "managed_mcp_hot_swap_e2e.ts"),
    ("managed_mcp_recovery", "managed_mcp_recovery_e2e.ts"),
)
REQUIRED_STAGE_ENVIRONMENTS = (
    (
        "acp_projected_container",
        "acp_projected_container_e2e.mjs",
        "AWAKEN_E2E_REQUIRE_CONTAINER",
        "1",
    ),
)
REQUIRED_DETERMINISTIC_EDGES = (
    ("test:compatibility", "test:sdk-behavior-owners"),
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
    "provider_environment.sh --self-test",
    "npm --prefix e2e run test:runner",
    "check_public_api.sh --require-tools",
    "scripts/contract/generate-contracts.sh --check",
    "pnpm --filter @awaken/managed-sdk-oracle check",
    "cargo deny --log-level error check advisories bans",
    "check_formal.sh --require-tools",
    "check-reliability.sh --require-tools",
    "pg_tests.sh --require-docker",
    "scripts/e2e/k8s_container_e2e.sh",
    "AWAKEN_K3D_REQUIRED=1 e2e/k3d/distributed_control_e2e.sh",
    "AWAKEN_K3D_REQUIRED=1 e2e/k3d/nats_wake_e2e.sh 12",
    "e2e/k3d/nats_wake_e2e.sh --self-test",
    "e2e/k3d/distributed_control_e2e.sh --self-test",
    "scripts/ci/e2e-coverage.sh",
    'run frontend "browser-e2e" pnpm -C web test:e2e',
    "npm --prefix e2e run test:sdk-latest-canary",
    "sandbox_capability_suite.sh --require-substrates",
)
REQUIRED_RELEASE_GROUPS = (
    "static",
    "docs",
    "rust",
    "api",
    "contracts",
    "formal",
    "reliability",
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
INGRESS_CONFORMANCE = "crates/server/awaken-dispatch/tests/dispatch_conformance.rs"
NEXTEST_CONFIG = ".config/nextest.toml"
# A test may wait, but it must say how long. The runner's deadline stops a
# stuck test from taking the suite with it; it cannot tell you what the test
# was waiting for, and it spends five minutes not telling you. The debt is
# cleared, so the baseline is zero and this reads as a refusal.
UNBOUNDED_WAIT_BASELINE = 0
# HTTP tests in the Managed protocol crate must name the status a body arrived
# with. Discarding it is how a route that had begun answering 404 went
# unnoticed: the test read the empty body and then waited for work the failed
# request never scheduled, and parked forever. `support::Call` makes that a
# type error -- it has no accessor that yields the body alone -- so every
# helper returns one and the status-tuple shape is refused outright.
MANAGED_HTTP_TESTS = "crates/server/awaken-protocol-managed/tests"
TUPLE_HELPER_BASELINE = 0
HTTP_HELPER_RETURN = re.compile(r"->\s*\(StatusCode,\s*Value\)")
RUST_FN = re.compile(r"\bfn\s+(\w+)")
TUPLE_HELPER = re.compile(r"->\s*\((?:axum::http::)?StatusCode\s*,")
TEST_ATTRIBUTE = re.compile(
    r"#\[(?:tokio::)?test[^\]]*\]\s*(?:#\[[^\]]*\]\s*)*(?:async\s+)?fn\s+(\w+)"
)
UNBOUNDED_WAIT = re.compile(r"\.(?:notified|recv|acquire|changed)\(\)\s*\.await")
BOUNDED_WAIT = re.compile(r"time::timeout|tokio::select!|select!|signaled\(")
# A unit or integration test that has not finished in five minutes is not slow,
# it is stuck. The bound exists so no configuration edit can restore the
# unbounded run that once parked a gate for eight hours.
MAX_TEST_DEADLINE_SECONDS = 300.0
DURATION = re.compile(r"(?P<value>\d+(?:\.\d+)?)\s*(?P<unit>ms|s|m|h)\Z")
DURATION_UNITS = {"ms": 0.001, "s": 1.0, "m": 60.0, "h": 3600.0}
QUALITY_WORKFLOW = ".github/workflows/quality.yml"
RELEASE_WORKFLOW = ".github/workflows/release.yml"
# A workflow other than the canonical one that runs a suite command is running a
# copy of release-gate logic. `sdk-drift-canary.yml` is exactly that: it drives
# the same official-SDK canaries the release gate drives, on a schedule, and
# nothing said the two spellings had to agree. Every such command must be the
# same string check-all.sh uses, or be registered here with why it differs.
#
# Key: the exact command. Value: why the release manifest does not own it.
WORKFLOW_ONLY_COMMANDS: dict[str, str] = {
    "pnpm --filter @awaken/managed-sdk-oracle check:python:online": (
        "The online variant of the oracle check: it queries PyPI for a newer "
        "`anthropic` release. The release manifest deliberately runs the "
        "hermetic `check` instead, because a gate must not fail on somebody "
        "else's publish. Running the online one on a schedule is the point of "
        "the canary; it is not a second copy of the gate's command."
    ),
}
# The command shapes a workflow can use to run a repository suite. Matched
# anywhere on the line, not anchored: these appear inside `run: |` blocks with
# redirections, `| tee` and line continuations after them, and an anchored
# pattern silently matched none of them.
WORKFLOW_SUITE_COMMAND = re.compile(
    r"(npm --prefix \S+ run \S+|pnpm --filter \S+ \S+|pnpm -C \S+ \S+|scripts/ci/[\w.-]+\.sh)"
)
K8S_CONTAINER_GATE = "scripts/e2e/k8s_container_e2e.sh"


def e2e_sources(root: Path, suffixes: set[str]) -> Iterator[Path]:
    """Only repository E2E sources; installed packages are never test owners."""
    for directory, children, files in os.walk(root / "e2e"):
        children[:] = [child for child in children if child != "node_modules"]
        for name in files:
            path = Path(directory) / name
            if path.suffix in suffixes:
                yield path


def e2e_files(root: Path) -> set[str]:
    return {
        path.relative_to(root / "e2e").as_posix()
        for path in e2e_sources(root, {".js", ".mjs", ".ts"})
        if "e2e" in path.stem
    }


def scenario_port_errors(sources: dict[str, str]) -> list[str]:
    """Reject callers that bypass the harness-owned non-ephemeral port block."""

    fixed_port = re.compile(
        r"(?:withScenarioServer\(\s*[^,\n]+,\s*[^,\n]+|"
        r"(?:withRealServer|withServer)\(\s*[^,\n]+)"
        r"\s*,\s*[0-9][0-9_]*\s*,",
    )
    return [
        f"{relative} passes a fixed port to the shared server harness; use E2E_PORT"
        for relative, text in sorted(sources.items())
        if fixed_port.search(text)
    ]


def coverage_runner_texts(root: Path) -> dict[str, str]:
    """Load the canonical runners plus any competing E2E-local coverage shell."""

    runners = {
        relative: (root / relative).read_text(encoding="utf-8")
        for relative in SECONDARY_RUNNERS
    }
    for path in e2e_sources(root, {".sh"}):
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
    for owner, child in REQUIRED_DETERMINISTIC_EDGES:
        commands = re.split(r"\s*&&\s*", scripts.get(owner, ""))
        if f"npm run {child}" not in commands:
            errors.append(f"{owner} does not invoke {child}")

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
    for scenario_id, relative in REQUIRED_STAGE_SCENARIOS:
        binding = re.compile(
            rf"\{{\s*id:\s*'{re.escape(scenario_id)}'\s*,\s*"
            rf"file:\s*'e2e/{re.escape(relative)}'"
        )
        if not binding.search(scenario_text):
            errors.append(
                f"stage scenario graph does not bind {scenario_id} to e2e/{relative}"
            )
    for scenario_id, relative, name, value in REQUIRED_STAGE_ENVIRONMENTS:
        binding = re.compile(
            rf"\{{\s*id:\s*'{re.escape(scenario_id)}'\s*,\s*"
            rf"file:\s*'e2e/{re.escape(relative)}'\s*,\s*"
            rf"environment:\s*\{{[^}}]*\b{re.escape(name)}:\s*'{re.escape(value)}'",
        )
        if not binding.search(scenario_text):
            errors.append(
                f"stage scenario {scenario_id} does not require {name}={value}"
            )

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
        if (
            'IGNORE="$(python3 scripts/ci/check_changed_e2e_line_coverage.py --print-ignore-filename-regex)"'
            not in text
            or len(re.findall(r"^IGNORE=", text, re.MULTILINE)) != 1
        ):
            errors.append(f"{relative} does not consume the canonical coverage scope exactly once")
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


def release_gate_errors(check_all: str, public_api: str, check_rust: str) -> list[str]:
    errors = [
        f"check-all does not require: {command}"
        for command in REQUIRED_RELEASE_COMMANDS
        if command not in check_all
    ]
    if "npm --prefix e2e run test:deterministic" in check_all:
        errors.append("check-all duplicates deterministic E2E already owned by its coverage runner")
    if re.search(r'^excluded="[^"\n]+"', public_api, re.MULTILINE):
        errors.append("public API gate still excludes workspace crates")
    if 'readonly PUBLIC_API_TOOL_VERSION="0.52.0"' not in public_api:
        errors.append("public API gate does not pin cargo-public-api 0.52.0")
    if 'readonly PUBLIC_API_NIGHTLY="nightly-2026-07-14"' not in public_api:
        errors.append("public API gate does not pin its dated nightly")
    if "cargo +nightly public-api" in public_api:
        errors.append("public API gate still uses a moving nightly")
    for group in REQUIRED_RELEASE_GROUPS:
        if not re.search(rf"^run {re.escape(group)} ", check_all, re.MULTILINE):
            errors.append(f"check-all has no executable {group} group")
    if "duration_seconds" not in check_all or "AWAKEN_TEST_TIMINGS_FILE" not in check_all:
        errors.append("check-all does not publish a configurable timing artifact")
    sdk_behavior = "npm --prefix e2e run test:sdk-behavior-owners"
    if sdk_behavior in check_all:
        errors.append(
            "check-all duplicates SDK behavior already owned by deterministic compatibility"
        )
    authority_arithmetic = "python3 scripts/ci/check_authority_arithmetic.py"
    if re.search(
        r'^run \S+ "[^"]+" python3 scripts/ci/check_authority_arithmetic\.py$',
        check_all,
        re.MULTILINE,
    ):
        errors.append(
            "check-all duplicates authority arithmetic already owned by check-rust"
        )
    if authority_arithmetic not in check_rust:
        errors.append("check-rust does not own the authority-arithmetic check")
    return errors


# A hook that runs only when certain files are staged is a gate whose trigger
# set must contain its input set: a check that reads `e2e/package.json` but
# fires only on `packages/managed-sdk-oracle/**` passes by never running when
# that manifest changes.
#
# The previous answer was a registry here that named each command and the extra
# files it reads. That registry could go stale in exactly the way it existed to
# prevent -- nothing made anyone add a row -- so it is gone. The rule below is
# structural instead: a checker declares its own inputs as INPUT_GLOBS, that
# declaration is the *only* author of the command's glob, and this refuses both
# a hand-written drift from the rendered glob and a declaration nothing runs.
def generated_hook_glob_errors(
    hooks: str, declarations: dict[str, tuple[str, ...]]
) -> list[str]:
    """Cause/effect for the generated trigger globs:

    | Rule | generated regions match the render | every declaring checker wired | Effect |
    |------|------------------------------------|-------------------------------|--------|
    | N1   | yes                                | yes                           | accept |
    | N2   | no (a glob was hand-edited or is stale) | yes                      | reject |
    | N3   | yes                                | no                            | reject |

    N4 is the admitted case and needs no row: a command that runs a checker
    which declares nothing is not governed at all, and its hand-written glob is
    left exactly as authored.
    """

    errors: list[str] = []
    if generate_hooks.render(hooks, declarations) != hooks:
        errors.append(
            "lefthook.yml trigger globs are hand-edited or stale; regenerate them with "
            "python3 scripts/ci/generate_hooks.py"
        )
    unwired = sorted(set(declarations) - generate_hooks.wired_scripts(hooks))
    for script in unwired:
        errors.append(
            f"scripts/ci/{script} declares INPUT_GLOBS but no lefthook pre-commit "
            "command runs it, so the declaration triggers nothing"
        )
    return errors


def k8s_container_gate_errors(gate: str) -> list[str]:
    """Require the canonical live K8s runner to execute its control-path case."""

    errors: list[str] = []
    if 'CONTROL_IMAGE="${AWAKEN_K8S_CONTROL_IMAGE:-}"' not in gate or 'k3d_validate_image_digest "$CONTROL_IMAGE"' not in gate:
        errors.append("Kubernetes gate does not require an exact control-image digest")
    if 'CONTROL_IMAGE=$(k3d_image_digest "$SESSION_IMAGE")' not in gate or (
        'build.sh" "$SESSION_IMAGE" ""' not in gate
    ):
        errors.append("Kubernetes gate cannot qualify the current source without an external image override")
    if not re.search(r'^k3d_create_cluster "\$CLUSTER" 0$', gate, re.MULTILINE):
        errors.append("Kubernetes gate must use the nested-node reserve with shared capacity admission")
    if 'k3d_import_storage_images "$CLUSTER" "$FIXTURE_IMAGE" "$SESSION_IMAGE" "$CONTROL_IMAGE"' not in gate:
        errors.append("Kubernetes gate does not import the exact control image")
    if not re.search(
        r'AWAKEN_K8S_E2E=1 AWAKEN_K8S_CONTROL_E2E=1\s*\\?\s*\n'
        r'\s*AWAKEN_K8S_CONTROL_IMAGE="\$CONTROL_IMAGE"',
        gate,
    ):
        errors.append("Kubernetes gate can still skip the required control roundtrip")
    return errors


def e2e_lock_owner_errors(pnpm_lock: str) -> list[str]:
    if re.search(r"^  e2e:$", pnpm_lock, re.MULTILINE):
        return ["E2E dependencies have a duplicate pnpm importer beside package-lock.json"]
    return []


def workflow_gate_errors(workflows: dict[str, str]) -> list[str]:
    """Require one exact-SHA workflow to execute the canonical full gate."""

    errors: list[str] = []
    quality = workflows.get(QUALITY_WORKFLOW, "")
    release = workflows.get(RELEASE_WORKFLOW, "")
    full_job = re.search(r"^  full_quality:\n(?:(?!^  \S).|\n)*", quality, re.MULTILINE)
    portability = re.search(r"^  trusted_host_portability:\n(?:(?!^  \S).|\n)*", quality, re.MULTILINE)
    if portability:
        calls = re.findall(
            r"^        run: scripts/ci/check-trusted-host-portability\.sh.*$",
            portability.group(0), re.MULTILINE,
        )
        expected = "run: scripts/ci/check-trusted-host-portability.sh ${{ runner.os == 'Linux' && '--linux-supervision' || '' }}"
        if calls != ["        " + expected]:
            errors.append("portability matrix must select Linux supervision through one canonical call")
        step = re.search(
            r"^      - name: Verify TrustedHost filesystem and native Sandbox contracts\n"
            r"(?:(?!^      - ).|\n)*", portability.group(0), re.MULTILINE,
        )
        if (not step or expected not in step.group(0)
            or re.search(r"^        if:", step.group(0), re.MULTILINE)
            or re.search(r"^    if:", portability.group(0), re.MULTILINE)):
            errors.append("portability matrix may not conditionally skip its canonical gate")
    for trigger in ("workflow_call:", "pull_request:", "push:", "workflow_dispatch:"):
        if trigger not in quality:
            errors.append(f"quality workflow does not declare {trigger}")
    required_quality = (
        "runs-on: [self-hosted, linux, x64, awaken-full-gate]",
        "persist-credentials: false",
        'source_revision="$(git rev-parse "$GITHUB_SHA^{commit}")"',
        'test "$(git rev-parse HEAD)" = "$source_revision"',
        "AWAKEN_TEST_TIMINGS_FILE: target/test-timings/check-all.tsv",
        "run: scripts/ci/check-all.sh --full-gate --record",
        "pnpm install --frozen-lockfile",
        "npm ci --prefix e2e",
    )
    for marker in required_quality:
        if not full_job or marker not in full_job.group(0):
            errors.append(f"quality workflow does not require: {marker}")
    evidence_step = re.search(
        r"^      - name: Retain exact-SHA quality evidence\n"
        r"(?:(?!^      - ).|\n)*",
        full_job.group(0) if full_job else "",
        re.MULTILINE,
    )
    for marker in (
        "        if: always()",
        "        uses: actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02",
        "          name: quality-${{ github.sha }}-${{ github.run_attempt }}",
        "            target/test-timings/",
        "            web/test-results/",
        "            scripts/ci/gate_record.json",
        "          if-no-files-found: error",
    ):
        if not evidence_step or marker not in [line.split(" #", 1)[0] for line in evidence_step.group(0).splitlines()]:
            errors.append(f"full quality evidence step does not require: {marker.strip()}")
    gate_step = re.search(
        r"^      - name: Run the one complete repository gate\n"
        r"(?:(?!^      - ).|\n)*",
        full_job.group(0) if full_job else "",
        re.MULTILINE,
    )
    if (
        quality.count("run: scripts/ci/check-all.sh") != 1
        or not gate_step
        or not re.search(r"^        run: scripts/ci/check-all\.sh --full-gate --record$", gate_step.group(0), re.MULTILINE)
    ):
        errors.append("quality workflow must invoke the exact canonical full gate once")
    if (
        full_job and re.search(r"^    if:", full_job.group(0), re.MULTILINE)
        or gate_step and re.search(r"^        if:", gate_step.group(0), re.MULTILINE)
    ):
        errors.append("quality workflow may not conditionally skip the full gate")
    if "check-all.sh --group" in quality or "check-rust.sh --full" in quality:
        errors.append("quality workflow contains a partial or parallel test manifest")
    if "continue-on-error:" in quality:
        errors.append("quality workflow may not suppress a gate failure")

    local_quality_call = "uses: ./.github/workflows/quality.yml"
    if release.count(local_quality_call) != 1:
        errors.append("release workflow must reuse the canonical quality workflow exactly once")
    evidence_download = re.search(
        r"^      - name: Download the exact quality invocation evidence\n(?:(?!^      - ).|\n)*",
        release, re.MULTILINE,
    )
    # Upload's declared paths share checkout root; the named download must
    # retain it so the one package proof consumer resolves its original path.
    if (not evidence_download or not re.search(r"^          path: \.$", evidence_download.group(0), re.MULTILINE)
        or "AWAKEN_FORMAL_RUN_EVIDENCE: target/test-timings/formal-run.json" not in release):
        errors.append("quality evidence download must preserve checkout root and canonical proof-run path")
    if "check-all.sh" in release or "check-rust.sh --full" in release:
        errors.append("release workflow duplicates the canonical quality workflow")
    if release.count("npm ci --prefix e2e") != 1 or "pnpm -C e2e install" in release:
        errors.append("release workflow must use the canonical E2E npm lock exactly once")

    gate_owners = sorted(
        relative
        for relative, workflow in workflows.items()
        if "scripts/ci/check-all.sh" in workflow
    )
    if gate_owners != [QUALITY_WORKFLOW]:
        errors.append(
            "canonical full gate must have exactly one workflow owner: "
            + ", ".join(gate_owners)
        )
    errors += workflow_only_registry_errors(workflows)
    return errors


def workflow_command_copy_errors(workflows: dict[str, str], check_all: str) -> list[str]:
    """W2: a second workflow may not keep its own spelling of a gate command.

    The canary workflow runs the release gate's official-SDK checks on a
    schedule. Nothing tied the two spellings together, so the gate could be
    changed and the canary would keep watching the old thing -- reporting green
    about a command the release gate no longer runs.
    """

    errors: list[str] = []
    for relative, workflow in sorted(workflows.items()):
        if relative in (QUALITY_WORKFLOW, RELEASE_WORKFLOW):
            continue
        for command in WORKFLOW_SUITE_COMMAND.findall(workflow):
            command = command.strip()
            # Compared to end of line, not by substring: `test:sdk-latest` is a
            # prefix of `test:sdk-latest-canary`, and a rule that cannot tell
            # those apart is the rule that let the two lanes drift.
            if f"{command}\n" in check_all or command in WORKFLOW_ONLY_COMMANDS:
                continue
            errors.append(
                f"{relative}: runs `{command}`, which scripts/ci/check-all.sh does not run. A "
                "second workflow holding its own spelling of a suite command is a copy of "
                "release-gate logic that nothing keeps in step; use the manifest's exact "
                "command or register it in WORKFLOW_ONLY_COMMANDS with why it must differ"
            )
    return errors


def workflow_only_registry_errors(workflows: dict[str, str]) -> list[str]:
    """A registered workflow-only command that no workflow runs is stale."""

    errors = []
    for command, reason in sorted(WORKFLOW_ONLY_COMMANDS.items()):
        if any(
            command in workflow
            for relative, workflow in workflows.items()
            if relative not in (QUALITY_WORKFLOW, RELEASE_WORKFLOW)
        ):
            continue
        errors.append(
            f"WORKFLOW_ONLY_COMMANDS names `{command}`, which no workflow runs any more; drop "
            f"the stale entry ({reason.split('.')[0]})"
        )
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


# The two commands `--push` exists to run: workspace clippy under
# `-D warnings` (dead code has turned main red after a green pre-commit) and
# the formal coverage ledger (a Kani harness the derived plan cannot select
# fails the formal group hours later; the ledger check derives the plan in
# seconds).
# Static scans the Rust gate owns: each must run exactly once, from the gate
# rather than from a hook of its own, and must not become a successful skip when
# Cargo is absent. The hook that delegates to the gate must also trigger on the
# checker's own source, or editing the checker never re-runs it.
GATE_OWNED_CHECKERS = (
    "check_dependency_sources.py",
    "check_crate_boundaries.py",
    "check_feature_matrix.py",
)

# The two ledger checks `--push` adds on top of what `--full` compiles. These
# are push-only by design: they answer in seconds and their answers come hours
# later otherwise. Everything else `--push` runs is *derived* from `--full`
# below, never listed here.
PUSH_ONLY_COMMANDS = (
    "python3 scripts/ci/check_formal_coverage.py",
    "python3 scripts/ci/check_formal_surface.py --require-complete",
)
# A compile-or-lint command, as it appears in the gate, up to its first quoted
# argument (`"${locked[@]}"`), which is where the two lanes stop being textually
# identical.
GATE_CARGO_COMMAND = re.compile(r"^\s*(cargo (?:clippy|check) --workspace [^\n\"]*)", re.MULTILINE)
# The compile surface a lint must cover. Narrowing it is the defect: the gate
# once compiled 108 features with `cargo check` and linted one named feature
# with clippy, so `-D warnings` had never been applied to most of what compiled.
MAXIMAL_COMPILE_SURFACE = "--workspace --all-features --all-targets"


def gate_branch(gate: str, pattern: str) -> str:
    """One `case` arm of check-rust.sh, by its arm pattern."""

    match = re.search(rf"(?ms)^  {re.escape(pattern)}\)\n(.*?)^    ;;", gate)
    return match.group(1) if match else ""


def push_gate_commands(gate: str) -> tuple[str, ...]:
    """What `--push` must run, derived from what `--full` runs.

    Two literal lists is how the lanes diverged: somebody widened the full
    gate's compile surface and nobody widened the push gate's, so a commit could
    leave a machine having satisfied a strictly weaker check than the one that
    would judge it. The push list is therefore not written down at all -- it is
    read off the `--full` arm, plus the two ledger checks push adds.
    """

    full = gate_branch(gate, "--full")
    derived = [command.rstrip() for command in GATE_CARGO_COMMAND.findall(full)]
    return tuple(dict.fromkeys(derived)) + PUSH_ONLY_COMMANDS


def rust_hook_errors(hooks: str, gate: str) -> list[str]:
    """Rust hook composition delegates each scan to its existing gate.

    Pre-commit runs `--quick`; pre-push runs `--push`, which is `--quick` plus
    everything `--full` compiles or lints plus the two ledger checks. The gate
    must actually run those commands on its `--push` path: a hook that names a
    mode the script does not implement, or a mode that silently drops one of the
    commands, keeps the pre-commit latency and loses the reason the mode exists.

    The list is derived from the `--full` arm rather than restated, so widening
    the full gate's compile surface without widening the push gate's is a
    failure here instead of a divergence nobody notices.
    """
    errors = []
    pre_commit = hooks.split("pre-commit:", 1)[-1].split("\npre-push:", 1)[0]
    pre_push = hooks.split("\npre-push:", 1)[-1] if "\npre-push:" in hooks else ""
    owner = re.search(r"(?m)^    rust-quick:\n((?:      .*\n)+)", pre_commit)
    owner_text = owner.group(1) if owner else ""
    if owner_text.count("run: scripts/ci/check-rust.sh --quick") != 1:
        errors.append("Rust pre-commit must delegate to the canonical gate once")
    push_owner = re.search(r"(?m)^    rust-quick:\n((?:      .*\n)+)", pre_push)
    push_text = push_owner.group(1) if push_owner else ""
    if push_text.count("run: scripts/ci/check-rust.sh --push") != 1:
        errors.append("Rust pre-push must delegate to the canonical gate's --push mode once")
    if "check-rust.sh --quick" in pre_push:
        errors.append("Rust pre-push still runs --quick, which skips clippy and the formal ledger")
    public_api_command = "run: scripts/ci/check_public_api.sh --require-tools"
    if pre_push.count(public_api_command) != 1:
        errors.append(
            "pre-push must invoke the canonical fail-closed public API gate exactly once"
        )
    push_branch = re.search(r"(?ms)^  --quick\|--push\)\n(.*?)^    ;;", gate)
    push_body = push_branch.group(1) if push_branch else ""
    if not push_branch or 'if [ "$mode" = "--push" ]' not in push_body:
        errors.append("check-rust.sh must implement --push as --quick plus push-only steps")
    derived = push_gate_commands(gate)
    if len(derived) == len(PUSH_ONLY_COMMANDS):
        errors.append(
            "check-rust.sh --full compiles or lints nothing this rule can derive; the push "
            "gate would then be derived from an empty set and assert nothing"
        )
    for command in derived:
        if command not in push_body:
            errors.append(
                f"check-rust.sh --push must run {command} — it is what --full runs, and a push "
                "gate weaker than the gate that judges the commit lets the difference reach main"
            )
    if "-D warnings" not in push_body:
        errors.append("check-rust.sh --push clippy must deny warnings")
    errors += lint_surface_errors(gate)
    cargo_gate = gate.find("if ! command -v cargo")
    for checker in GATE_OWNED_CHECKERS:
        command = f"python3 scripts/ci/{checker}"
        if command in pre_commit:
            errors.append(f"Rust pre-commit duplicates {checker}")
        if gate.count(command) != 1:
            errors.append(f"Rust gate must execute {checker} exactly once")
        if f"scripts/ci/{checker}" not in owner_text:
            errors.append(f"Rust hook does not cover changes to {checker}")
        if cargo_gate < 0 or gate.find(command) > cargo_gate:
            errors.append(f"Cargo absence must not suppress {checker}")
    return errors


def lint_surface_errors(gate: str) -> list[str]:
    """`-D warnings` must be applied to the maximal graph in every judging lane.

    The gate compiled 108 optional features with `cargo check` and linted a
    single named feature with clippy, so everything it proved *compiles* was
    never proved warning-free. The two commands were independent literals, which
    is why nobody noticed they had stopped agreeing.

    The rule is over the surface, not the command: a lane that lints at all must
    lint `--workspace --all-features --all-targets`. `--quick` may still compile
    a narrower graph for speed -- that surface is a strict subset of the maximal
    one, so it cannot hide anything the judging lanes will not see.
    """

    errors: list[str] = []
    for arm, must_lint in (("--quick|--push", True), ("--full", True)):
        body = gate_branch(gate, arm)
        if not body:
            errors.append(f"check-rust.sh has no {arm} arm to check the lint surface of")
            continue
        linted = {
            command.split("cargo clippy ", 1)[1].strip()
            for command in GATE_CARGO_COMMAND.findall(body)
            if command.strip().startswith("cargo clippy")
        }
        if must_lint and not linted:
            errors.append(
                f"check-rust.sh {arm} compiles the workspace but runs no workspace clippy; "
                "a lane that judges a commit must lint what it compiles"
            )
            continue
        if MAXIMAL_COMPILE_SURFACE not in linted:
            errors.append(
                f"check-rust.sh {arm} lints {sorted(linted)} instead of "
                f"`{MAXIMAL_COMPILE_SURFACE}`; clippy must cover the same feature set "
                "cargo check covers, or the gate must state and enforce why a narrower set "
                "is correct"
            )
    return errors


def deadline_seconds(slow_timeout: object) -> float | None:
    """The wall-clock bound a slow-timeout table puts on one test.

    ``period`` alone only makes nextest *report* a slow test; without
    ``terminate-after`` the test still runs forever. Return None whenever the
    table states no bound, so the caller rejects rather than guesses.
    """

    if not isinstance(slow_timeout, dict):
        return None
    terminate = slow_timeout.get("terminate-after")
    if not isinstance(terminate, int) or isinstance(terminate, bool) or terminate < 1:
        return None
    match = DURATION.fullmatch(str(slow_timeout.get("period", "")).strip())
    if match is None:
        return None
    return float(match["value"]) * DURATION_UNITS[match["unit"]] * terminate


def test_deadline_errors(config_text: str, rust_gate: str) -> list[str]:
    """Require one runner-enforced deadline on every Rust test.

    Neither libtest nor ``cargo test`` can bound a single test, so one test
    awaiting a signal nobody will send parks the entire gate with no output and
    no end. nextest can, and this keeps that the only way the suite runs:
    the profile must terminate a test, no override may relax that past the
    bound, and the gate may not fall back to an unbounded runner.
    """

    errors: list[str] = []
    try:
        config = tomllib.loads(config_text)
    except tomllib.TOMLDecodeError as error:
        return [f"{NEXTEST_CONFIG} does not parse: {error}"]

    profiles = config.get("profile", {})
    default = profiles.get("default", {}) if isinstance(profiles, dict) else {}
    bound = deadline_seconds(default.get("slow-timeout"))
    if bound is None:
        errors.append(
            f"{NEXTEST_CONFIG} [profile.default] must set slow-timeout with "
            "terminate-after; period alone only reports a slow test"
        )
    elif bound > MAX_TEST_DEADLINE_SECONDS:
        errors.append(
            f"{NEXTEST_CONFIG} [profile.default] deadline is {bound:.0f}s, "
            f"above the {MAX_TEST_DEADLINE_SECONDS:.0f}s bound; bound the wait "
            "inside the test instead of widening the runner"
        )
    # The gate filters nextest's stream through grep, so a failure seen only
    # under the full run has no panic text unless the final summary reprints
    # it. "immediate-final" is the only setting that both shows it live and
    # keeps it after the run; "immediate" alone loses it to the filter.
    if default.get("failure-output") != "immediate-final":
        errors.append(
            f"{NEXTEST_CONFIG} [profile.default] must set failure-output = "
            '"immediate-final" so a failing test\'s captured output is '
            "reprinted at the end of the run"
        )
    for name, profile in (profiles.items() if isinstance(profiles, dict) else ()):
        for index, override in enumerate(profile.get("overrides", []) or []):
            if "slow-timeout" not in override:
                continue
            relaxed = deadline_seconds(override["slow-timeout"])
            if relaxed is None or relaxed > MAX_TEST_DEADLINE_SECONDS:
                errors.append(
                    f"{NEXTEST_CONFIG} [profile.{name}] override {index} "
                    f"({override.get('filter', 'no filter')}) removes the "
                    "per-test deadline"
                )

    if not re.search(r"cargo nextest run --workspace", rust_gate):
        errors.append("check-rust.sh must run the workspace suite under nextest")
    unbounded = [
        line.strip()
        for line in rust_gate.splitlines()
        if re.search(r"(?<!nextest run )cargo test --workspace", line)
        and "--doc" not in line
        and not line.strip().startswith("#")
    ]
    if unbounded:
        errors.append(
            "check-rust.sh runs the workspace suite without a per-test "
            f"deadline: {unbounded[0]}"
        )
    if "cargo nextest --version" not in rust_gate:
        errors.append(
            "check-rust.sh must fail closed when cargo-nextest is absent "
            "instead of falling back to an unbounded runner"
        )
    return errors



def test_body(text: str, start: int) -> str:
    """The brace-balanced body of the test function declared at ``start``."""

    opening = text.find("{", start)
    if opening < 0:
        return ""
    depth, index = 0, opening
    while index < len(text):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return text[opening : index + 1]
        index += 1
    return text[opening:]


def unbounded_wait_errors(
    sources: dict[str, str], baseline: int = -1
) -> list[str]:
    """Ratchet the tests that wait on a signal without stating a deadline.

    Such a test does not fail when its producer stops running -- it parks and
    reports nothing. The runner now kills it, which bounds the damage but
    still leaves a test whose failure says "timed out" instead of naming what
    it waited for. The existing ones are counted, and the count may only fall.
    """

    if baseline < 0:
        baseline = UNBOUNDED_WAIT_BASELINE
    offenders: list[str] = []
    for relative, text in sorted(sources.items()):
        for match in TEST_ATTRIBUTE.finditer(text):
            # A paused-clock test cannot state a wall-clock deadline: its
            # runtime auto-advances virtual time whenever every task is idle,
            # so `tokio::time::timeout` there is not a bound but a trap that
            # always fires. The runner's real deadline is what bounds these.
            if "start_paused" in match.group(0):
                continue
            body = test_body(text, match.end())
            if UNBOUNDED_WAIT.search(body) and not BOUNDED_WAIT.search(body):
                offenders.append(f"{relative}::{match.group(1)}")
    if len(offenders) > baseline:
        listed = ", ".join(offenders[:5])
        return [
            f"{len(offenders)} tests wait without a deadline, baseline "
            f"{baseline}: {len(offenders) - baseline} "
            f"new one(s). Wrap the wait in tokio::time::timeout so the failure "
            f"names what it waited for. First: {listed}"
        ]
    if len(offenders) < baseline:
        return [
            f"only {len(offenders)} tests wait without a deadline, below the "
            f"{baseline} baseline; lower UNBOUNDED_WAIT_BASELINE "
            "to hold the ground"
        ]
    return []



def http_helpers(text: str) -> set[str]:
    """Names of the functions in one test file that return a status and a body.

    Taken by looking back from each `-> (StatusCode, Value)` to the nearest
    preceding `fn`, because these signatures wrap across lines.
    """

    names: set[str] = set()
    for match in HTTP_HELPER_RETURN.finditer(text):
        declared = RUST_FN.findall(text[: match.start()])
        if declared:
            names.add(declared[-1])
    return names


def discarded_status_errors(sources: dict[str, str]) -> list[str]:
    """Refuse an HTTP test that reads a body without naming its status.

    Two rules, because they fail differently. Discarding the status is the
    defect itself and is refused outright. Returning the bare tuple is only
    what makes the defect writable, and there is too much of it to refuse at
    once, so it is counted and the count may only fall.
    """

    errors: list[str] = []
    discards: list[str] = []
    for relative, text in sorted(sources.items()):
        helpers = http_helpers(text)
        if not helpers:
            continue
        # Only a call to one of this file's HTTP helpers carries a status to
        # discard. Other two-value returns here mean something else entirely --
        # a store generation, a pair of handles -- and ignoring their first
        # element is not this defect.
        discard = re.compile(
            r"^[^/\n]*let \(_, \w+\) =\s*(?:"
            + "|".join(sorted(re.escape(name) for name in helpers))
            + r")\(",
            re.MULTILINE,
        )
        discards.extend(
            f"{relative}:{text[: match.start()].count(chr(10)) + 1}"
            for match in discard.finditer(text)
        )
    if discards:
        errors.append(
            f"{len(discards)} HTTP tests read a body without naming the status "
            f"it arrived with, starting at {discards[0]}. Assert the status, or "
            "return a response type whose body needs one (see tests/dreams.rs)."
        )
    helpers = sum(len(TUPLE_HELPER.findall(text)) for text in sources.values())
    if helpers > TUPLE_HELPER_BASELINE:
        errors.append(
            f"{helpers} test helper(s) return a status tuple a caller can "
            "destructure past. Return support::Call, whose body is only "
            "reachable through expect(status)."
        )
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
        scenario_port_errors(
            {
                path.relative_to(root).as_posix(): path.read_text(encoding="utf-8")
                for path in e2e_sources(root, {".js", ".mjs", ".ts"})
            }
        )
    )
    errors.extend(
        release_gate_errors(
            (root / "scripts/ci/check-all.sh").read_text(encoding="utf-8"),
            (root / "scripts/ci/check_public_api.sh").read_text(encoding="utf-8"),
            (root / "scripts/ci/check-rust.sh").read_text(encoding="utf-8"),
        )
    )
    errors.extend(
        k8s_container_gate_errors(
            (root / K8S_CONTAINER_GATE).read_text(encoding="utf-8")
        )
    )
    errors.extend(rust_hook_errors(
        (root / "lefthook.yml").read_text(encoding="utf-8"),
        (root / "scripts/ci/check-rust.sh").read_text(encoding="utf-8"),
    ))
    errors.extend(
        generated_hook_glob_errors(
            (root / "lefthook.yml").read_text(encoding="utf-8"),
            generate_hooks.declared_input_globs(root / "scripts/ci"),
        )
    )
    errors.extend(
        test_deadline_errors(
            (root / NEXTEST_CONFIG).read_text(encoding="utf-8"),
            (root / "scripts/ci/check-rust.sh").read_text(encoding="utf-8"),
        )
    )
    errors.extend(
        discarded_status_errors(
            {
                path.relative_to(root).as_posix(): path.read_text(encoding="utf-8")
                for path in (root / MANAGED_HTTP_TESTS).rglob("*.rs")
            }
        )
    )
    errors.extend(
        unbounded_wait_errors(
            {
                path.relative_to(root).as_posix(): path.read_text(
                    encoding="utf-8", errors="replace"
                )
                for path in (root / "crates").rglob("*.rs")
            }
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
    errors.extend(
        workflow_gate_errors(
            {
                path.relative_to(root).as_posix(): path.read_text(encoding="utf-8")
                for path in (root / ".github" / "workflows").glob("*.yml")
            }
        )
    )
    errors.extend(
        workflow_command_copy_errors(
            {
                path.relative_to(root).as_posix(): path.read_text(encoding="utf-8")
                for path in (root / ".github" / "workflows").glob("*.yml")
            },
            (root / "scripts/ci/check-all.sh").read_text(encoding="utf-8"),
        )
    )
    errors.extend(e2e_lock_owner_errors((root / "pnpm-lock.yaml").read_text(encoding="utf-8")))
    return errors


def self_test() -> None:
    # Per-test deadline decision table. C1 the default profile terminates a
    # test; C2 the deadline is within the bound; C3 no override removes it;
    # C4 the Rust gate runs the suite under nextest; C5 the gate runs no
    # unbounded workspace suite; C6 the gate fails closed without nextest;
    # C7 the default profile reprints a failure's output in the final summary.
    #
    # | Rule | C1 | C2 | C3 | C4 | C5 | C6 | C7 | Effect |
    # | D1   | T  | T  | T  | T  | T  | T  | T  | accept |
    # | D2   | F  | *  | *  | T  | T  | T  | T  | reject reporting-only timeout |
    # | D3   | T  | F  | *  | T  | T  | T  | T  | reject widened deadline |
    # | D4   | T  | T  | F  | T  | T  | T  | T  | reject relaxing override |
    # | D5   | T  | T  | T  | F  | T  | T  | T  | reject suite off the bounded runner |
    # | D6   | T  | T  | T  | T  | F  | T  | T  | reject unbounded workspace run |
    # | D7   | T  | T  | T  | T  | T  | F  | T  | reject silent fallback |
    # | D8   | T  | T  | T  | T  | T  | T  | F  | reject a failure whose output the filter eats |
    # A doctest line names the same binary but cannot hang a workspace suite
    # the way an async integration test can, so C5 exempts it by name.
    deadline_config = (ROOT / NEXTEST_CONFIG).read_text(encoding="utf-8")
    deadline_gate = (ROOT / "scripts/ci/check-rust.sh").read_text(encoding="utf-8")
    assert not test_deadline_errors(deadline_config, deadline_gate), "D1 bounded"
    assert test_deadline_errors(
        deadline_config.replace(", terminate-after = 5 }", " }"), deadline_gate
    ), "D2 period alone"
    assert test_deadline_errors(
        deadline_config.replace('period = "60s", terminate-after = 5', 'period = "60s", terminate-after = 10'),
        deadline_gate,
    ), "D3 widened"
    assert test_deadline_errors(
        deadline_config
        + '\n[[profile.default.overrides]]\nfilter = \'test(slow)\'\nslow-timeout = "10m"\n',
        deadline_gate,
    ), "D4 relaxing override"
    assert test_deadline_errors(
        deadline_config, deadline_gate.replace("cargo nextest run --workspace", "cargo test --workspace")
    ), "D5 off the bounded runner"
    assert test_deadline_errors(
        deadline_config, deadline_gate + "\n      cargo test --workspace\n"
    ), "D6 unbounded workspace run"
    assert test_deadline_errors(
        deadline_config, deadline_gate.replace("cargo nextest --version", "true")
    ), "D7 silent fallback"
    assert not test_deadline_errors(
        deadline_config, deadline_gate + "\n      cargo test --workspace --doc\n"
    ), "D1 doctest line is not an unbounded suite"
    assert 'failure-output = "immediate-final"' in deadline_config, "D8 fixture"
    for lost in ('failure-output = "immediate"', 'failure-output = "final"', ""):
        assert test_deadline_errors(
            deadline_config.replace('failure-output = "immediate-final"', lost),
            deadline_gate,
        ), f"D8 failure output {lost!r} does not survive the gate filter"
    # Unbounded-wait ratchet. W1 the tree is at the baseline -> accept;
    # W2 one more test waits without a deadline -> reject; W3 one fewer ->
    # reject until the baseline is lowered with it; W4 a wait the test bounds
    # itself is not counted.
    waiting = '#[tokio::test]\nasync fn waits() { gate.notified().await; }\n'
    bounded = ('#[tokio::test]\nasync fn waits() '
               '{ tokio::time::timeout(d, gate.notified()).await; }\n')
    at_baseline = {f"crates/x/src/w{index}.rs": waiting for index in range(3)}
    assert not unbounded_wait_errors(at_baseline, 3), "W1 baseline"
    assert unbounded_wait_errors({**at_baseline, "crates/x/src/new.rs": waiting}, 3), "W2 one more"
    assert unbounded_wait_errors(
        {key: value for index, (key, value) in enumerate(at_baseline.items()) if index}, 3
    ), "W3 one fewer without lowering the baseline"
    assert not unbounded_wait_errors({**at_baseline, "crates/x/src/ok.rs": bounded}, 3), "W4 bounded"
    # The repository is at zero, so the same rule reads as a plain refusal.
    assert not unbounded_wait_errors({}, 0), "W5 nothing waiting unbounded"
    assert unbounded_wait_errors({"crates/x/src/w.rs": waiting}, 0), "W6 refuse the first one"
    # Hook cause/effect table: H1 canonical nested owner + checker-edit trigger
    # + static-before-Cargo -> accept. H2 duplicate direct/nested invocation ->
    # reject. H3 missing nested invocation/trigger -> reject. H4 Cargo checked
    # first -> reject, because an absent toolchain must not hide static failures.
    # Cross H2/H3/H4 with both previously duplicated scans; no second test owner.
    hooks = (ROOT / "lefthook.yml").read_text(encoding="utf-8")
    gate = (ROOT / "scripts/ci/check-rust.sh").read_text(encoding="utf-8")
    assert not rust_hook_errors(hooks, gate), "H1 single owner"
    # Push-mode table. Causes: P1 pre-commit runs --quick; P2 pre-push runs
    # --push; P3 the gate's --push branch runs workspace clippy under
    # -D warnings; P4 it runs the formal coverage ledger; P5 pre-push still
    # names --quick; P6 the hook invokes the canonical fail-closed public API
    # gate exactly once. Effects: accept / reject.
    # | Rule | P1 | P2 | P3 | P4 | P5 | P6 | Effect |
    # | H5   | T  | T  | T  | T  | F  | T  | accept |
    # | H6   | F  | T  | T  | T  | F  | T  | reject: pre-commit off --quick |
    # | H7   | T  | F  | T  | T  | T  | T  | reject: pre-push still --quick |
    # | H8   | T  | F  | T  | T  | F  | T  | reject: pre-push names no gate mode |
    # | H9   | T  | T  | F  | T  | F  | T  | reject: --push without clippy |
    # | H10  | T  | T  | T  | F  | F  | T  | reject: --push without the ledger |
    # | H11  | T  | T  | T* | T  | F  | T  | reject: clippy present but warnings allowed |
    # | H12  | --full widens its compile/lint surface, --push does not | reject |
    # | H13  | the lint surface is narrower than the compiled surface  | reject |
    # | H14  | T  | T  | T  | T  | F  | F  | reject: public API unchecked |
    pre_push_at = hooks.index("\npre-push:")
    assert rust_hook_errors(
        hooks[:pre_push_at].replace("check-rust.sh --quick", "check-rust.sh --push") + hooks[pre_push_at:],
        gate,
    ), "H6 pre-commit must keep --quick"
    assert rust_hook_errors(
        hooks[:pre_push_at] + hooks[pre_push_at:].replace("check-rust.sh --push", "check-rust.sh --quick"),
        gate,
    ), "H7 pre-push must not fall back to --quick"
    assert rust_hook_errors(
        hooks[:pre_push_at] + hooks[pre_push_at:].replace("check-rust.sh --push", "check-rust.sh --full"),
        gate,
    ), "H8 pre-push must name --push"
    push_start = gate.index("  --quick|--push)")
    push_end = gate.index("    ;;", push_start)
    push_body = gate[push_start:push_end]
    clippy_command = "cargo clippy " + MAXIMAL_COMPILE_SURFACE
    assert clippy_command in push_body and "check_formal_coverage.py" in push_body, (
        "H5 the live --push branch carries both commands"
    )
    # H5b the push list is derived from --full, not restated: every compile or
    # lint command the full arm runs is in the push arm, and the derivation is
    # what says so.
    assert set(push_gate_commands(gate)) - set(PUSH_ONLY_COMMANDS), (
        "H5b the derivation must read at least one command off the --full arm"
    )
    for derived in push_gate_commands(gate):
        assert derived in push_body, f"H5b --push is missing the derived command {derived}"
    # H12 widen --full alone: the derivation makes that a failure, where two
    # literal lists let the lanes silently disagree.
    full_start = gate.index("  --full)")
    widened = gate[:full_start] + gate[full_start:].replace(
        clippy_command, "cargo clippy --workspace --all-features --all-targets --tests", 1
    )
    assert rust_hook_errors(hooks, widened), "H12 a wider --full must widen --push too"
    # H13 narrow the lint surface while the compile surface stays maximal.
    narrowed = gate.replace(clippy_command, "cargo clippy --workspace --all-targets")
    assert lint_surface_errors(narrowed), "H13 the lint surface must equal the compiled surface"
    assert not lint_surface_errors(gate), "H13 the live gate lints what it compiles"
    without_clippy = gate[:push_start] + push_body.replace(clippy_command, "true") + gate[push_end:]
    assert rust_hook_errors(hooks, without_clippy), "H9 --push must run workspace clippy"
    without_ledger = gate[:push_start] + push_body.replace("python3 scripts/ci/check_formal_coverage.py", "true") + gate[push_end:]
    assert rust_hook_errors(hooks, without_ledger), "H10 --push must run the formal ledger"
    allowing = gate[:push_start] + push_body.replace("-D warnings", "-W warnings") + gate[push_end:]
    assert rust_hook_errors(hooks, allowing), "H11 --push clippy must deny warnings"
    assert rust_hook_errors(
        hooks.replace("run: scripts/ci/check_public_api.sh --require-tools", "run: true"),
        gate,
    ), "H14 pre-push must run the canonical public API gate"
    assert rust_hook_errors(hooks, gate.replace("  --quick|--push)", "  --quick)")), "H8 gate without a --push branch"
    # Generated trigger-glob table, on a synthetic hook file.
    # | Rule | generated regions match the render | declaring checker wired | Effect |
    # | N1   | yes                                | yes                     | accept |
    # | N2   | no, one glob was hand-edited       | yes                     | reject |
    # | N3   | yes                                | no                      | reject |
    # | N4   | n/a, the invoked checker declares nothing | n/a              | admitted, glob untouched |
    declarations = {"check_alpha.py": ("docs/*.md",)}
    synthetic = generate_hooks.render(
        "pre-commit:\n  commands:\n    alpha:\n"
        "      run: python3 scripts/ci/check_alpha.py {staged_files}\n"
        "\n    shell:\n"
        '      glob: "crates/**/*.rs"\n'
        "      run: scripts/ci/check-rust.sh --quick\n",
        declarations,
    )
    assert not generated_hook_glob_errors(synthetic, declarations), "N1"
    hand_edited = synthetic.replace(
        '{docs/*.md,scripts/ci/check_alpha.py}', "docs/README.md"
    )
    assert hand_edited != synthetic and generated_hook_glob_errors(hand_edited, declarations), "N2"
    assert generated_hook_glob_errors(
        synthetic, {**declarations, "check_unwired.py": ("formal/*.json",)}
    ), "N3"
    assert '      glob: "crates/**/*.rs"\n' in synthetic and "check-rust.sh" in synthetic, "N4"
    # Live: the repository's own hooks are exactly what their declarations render.
    assert not generated_hook_glob_errors(
        hooks, generate_hooks.declared_input_globs(ROOT / "scripts/ci")
    ), "N1 live"
    for checker in GATE_OWNED_CHECKERS:
        command = f"python3 scripts/ci/{checker}"
        duplicate = hooks.replace("pre-commit:", f"pre-commit:\n    duplicate:\n      run: {command}", 1)
        assert rust_hook_errors(duplicate, gate), f"H2 direct {checker}"
        assert rust_hook_errors(hooks, gate + "\n" + command), f"H2 nested {checker}"
        assert rust_hook_errors(hooks, gate.replace(command, "true")), f"H3 missing {checker}"
        assert rust_hook_errors(hooks.replace(f"scripts/ci/{checker},", ""), gate), f"H3 trigger {checker}"
        assert rust_hook_errors(hooks, "if ! command -v cargo\n" + gate), f"H4 {checker}"
    # Cause/effect decision table for the checker itself:
    # C1 canonical aggregate includes every required package suite and required
    # stage scenario, including the full-chain patch/manifest/apply/test/review-ref
    # closure exactly once;
    # C2 every E2E belongs to package scripts or the stage cause/effect graph;
    # C3 secondary runners delegate to the aggregate; C4 secondary runners
    # preserve failure status; C5 functional obligation ids are unique; C6 the
    # release gate requires every external/API/dependency suite, including both
    # RustSec advisories and dependency-policy bans, and the canonical
    # deterministic-environment self-test without exclusions, while SDK behavior
    # and authority arithmetic each retain exactly one nested gate owner;
    # C7 every shared conformance testkit is executed by every production backend;
    # C8 deterministic leaf commands are unique; C9 stage scenarios have one owner;
    # C10 a scenario credited with container behavior explicitly requires that
    # substrate instead of returning a successful skip; C11 every
    # shared server-harness caller consumes the harness-owned non-ephemeral port
    # rather than retaining a 38xxx literal.
    # `test:compatibility` is part of C1, while the network-backed latest-SDK
    # canary is part of C6 so it runs once at the release boundary rather than
    # contaminating the hermetic deterministic runner.
    #
    # | Rule | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | C9 | C10 | Effect |
    # | R1   | T  | T  | T  | T  | T  | T  | T  | T  | T  | T   | accept |
    # | R2   | F  | *  | *  | *  | *  | *  | *  | *  | *  | *   | reject missing suite |
    # | R3   | T  | F  | *  | *  | *  | *  | *  | *  | *  | *   | reject unclassified E2E |
    # | R4   | T  | T  | F  | *  | *  | *  | *  | *  | *  | *   | reject parallel runner |
    # | R5   | T  | T  | T  | F  | *  | *  | *  | *  | *  | *   | reject swallowed failure |
    # | R6   | T  | T  | T  | T  | F  | *  | *  | *  | *  | *   | reject duplicate obligation |
    # | R7   | T  | T  | T  | T  | T  | F  | *  | *  | *  | *   | reject incomplete release gate |
    # | R8   | T  | T  | T  | T  | T  | T  | F  | *  | *  | *   | reject detached testkit/backend |
    # | R9   | T  | T  | T  | T  | T  | T  | T  | F  | *  | *   | reject duplicate command |
    # | R10  | T  | T  | T  | T  | T  | T  | T  | T  | F  | *   | reject package/stage overlap |
    # | R14  | T  | T  | T  | T  | T  | T  | T  | T  | T  | F   | reject false-green substrate |
    # Port decision table: P1 C11=true => accept; P2 C11=false => reject the
    # duplicate fixed-port authority before any scenario process starts.
    package = json.loads(PACKAGE.read_text(encoding="utf-8"))
    scripts: dict[str, str] = package["scripts"]
    deterministic_suites: list[str] = package["awakenTest"]["deterministicSuites"]
    stage_text = STAGE_GRAPH.read_text(encoding="utf-8")
    files = e2e_files(ROOT)
    runners = coverage_runner_texts(ROOT)
    # P3 source-boundary table: C1 project files and C2 nested project scripts
    # remain visible; C3 installed node_modules churns during npm ci. P3a
    # C1+C2 => enumerate both owners. P3b C3 => enumerate none of its files,
    # even when an installed script looks like an E2E test or coverage runner.
    with tempfile.TemporaryDirectory() as fixture_root:
        fixture_e2e = Path(fixture_root) / "e2e"
        (fixture_e2e / "k3d").mkdir(parents=True)
        (fixture_e2e / "node_modules" / "package").mkdir(parents=True)
        (fixture_e2e / "own_e2e.ts").write_text("", encoding="utf-8")
        (fixture_e2e / "k3d" / "own.sh").write_text("", encoding="utf-8")
        (fixture_e2e / "node_modules" / "package" / "foreign_e2e.ts").write_text("", encoding="utf-8")
        (fixture_e2e / "node_modules" / "package" / "foreign.sh").write_text("", encoding="utf-8")
        assert e2e_files(Path(fixture_root)) == {"own_e2e.ts"}, "P3a/P3b test source"
        assert [path.relative_to(fixture_e2e).as_posix() for path in e2e_sources(Path(fixture_root), {".sh"})] == ["k3d/own.sh"], "P3a/P3b runner source"
    errors = orchestration_errors(scripts, deterministic_suites, stage_text, files, runners)
    if errors:
        raise AssertionError("R1 repository fixture must be valid: " + "; ".join(errors))
    scenario_sources = {
        path.relative_to(ROOT).as_posix(): path.read_text(encoding="utf-8")
        for path in e2e_sources(ROOT, {".js", ".mjs", ".ts"})
    }
    assert not scenario_port_errors(scenario_sources), "P1 harness port authority"
    assert scenario_port_errors(
        {
            "e2e/fixed_e2e.mjs": "withScenarioServer('management', 'mcp', 38146, run)",
            "e2e/real_fixed_e2e.mjs": "withRealServer('echo', 38147, run)",
        }
    ) == [
        "e2e/fixed_e2e.mjs passes a fixed port to the shared server harness; use E2E_PORT",
        "e2e/real_fixed_e2e.mjs passes a fixed port to the shared server harness; use E2E_PORT",
    ], "P2 fixed shared-harness port"
    assert not scenario_port_errors(
        {
            "e2e/dynamic_e2e.mjs": """
                withRealServer('echo', Number(process.env.E2E_PORT), run);
                withRealServer('vision', Number(process.env.E2E_PORT), run);
                withScenarioServer(
                    'management', 'mcp', Number(process.env.E2E_PORT), run
                );
            """,
        }
    ), "P3 dynamic calls cannot merge into one false fixed-port match"

    # Scope rules S1: both runners consume the same denominator; S2: a local
    # regex or override creates a competing authority and must be rejected.
    for runner in SECONDARY_RUNNERS:
        drifted = dict(runners)
        drifted[runner] += "\nIGNORE='awaken-store-postgres/'\n"
        assert any(
            "canonical coverage scope" in error
            for error in orchestration_errors(
                scripts, deterministic_suites, stage_text, files, drifted
            )
        ), f"S2 duplicate scope in {runner}"

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
    detached_sdk_behavior = dict(scripts)
    detached_sdk_behavior["test:compatibility"] = detached_sdk_behavior[
        "test:compatibility"
    ].replace(" && npm run test:sdk-behavior-owners", "", 1)
    assert any(
        "test:compatibility does not invoke test:sdk-behavior-owners" in error
        for error in orchestration_errors(
            detached_sdk_behavior, deterministic_suites, stage_text, files, runners
        )
    ), "R2 SDK behavior remains under its sole deterministic compatibility owner"
    # Required-stage ownership table: C1=the canonical stage graph binds the
    # required scenario id to its executable file; C2=the package graph does
    # not execute that file separately. Effects: E1=C1+C2 accepts one owner;
    # E2=!C1 rejects missing evidence; E3=!C2 rejects duplicate execution.
    # Each required scenario takes R2 below, while the existing R10 mutation
    # covers E3 without creating a second list of scenario commands.
    #
    # | Rule | C1 | C2 | Effect |
    # | R1   | T  | T  | E1 accept one Stage owner |
    # | R2   | F  | *  | E2 reject missing Stage binding |
    # | R10  | T  | F  | E3 reject package/Stage overlap |
    for scenario_id, relative in REQUIRED_STAGE_SCENARIOS:
        missing_required_stage = stage_text.replace(
            f"id: '{scenario_id}'",
            f"id: '{scenario_id}_removed'",
            1,
        )
        assert any(
            f"does not bind {scenario_id}" in error
            for error in orchestration_errors(
                scripts,
                deterministic_suites,
                missing_required_stage,
                files,
                runners,
            )
        ), f"R2 {scenario_id} remains in the Stage aggregate"

    duplicate_full_chain = [*deterministic_suites, "test:full-chain"]
    assert any(
        "managed_full_chain_e2e.mjs" in error and "also execute" in error
        for error in orchestration_errors(
            scripts, duplicate_full_chain, stage_text, files, runners
        )
    ), "R10 full-chain closure has one stage owner"

    optional_container_stage = stage_text.replace(
        "environment: { AWAKEN_E2E_REQUIRE_CONTAINER: '1' },",
        "",
        1,
    )
    assert any(
        "acp_projected_container does not require AWAKEN_E2E_REQUIRE_CONTAINER=1" in error
        for error in orchestration_errors(
            scripts,
            deterministic_suites,
            optional_container_stage,
            files,
            runners,
        )
    ), "R14 stage coverage cannot credit an optional container skip"

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
    check_rust = (ROOT / "scripts/ci/check-rust.sh").read_text(encoding="utf-8")
    assert not release_gate_errors(check_all, public_api, check_rust), "R1/C6"
    # C11 coverage invokes the canonical deterministic suite once; C12 the
    # browser suite executes at the frontend boundary. Effects: duplicates or
    # omission reject the graph (R16/R17), rather than silently losing evidence.
    assert release_gate_errors(
        check_all + '\nrun e2e "duplicate" npm --prefix e2e run test:deterministic\n',
        public_api, check_rust,
    ), "R16 duplicate deterministic run"
    assert release_gate_errors(
        check_all.replace('run frontend "browser-e2e" pnpm -C web test:e2e', ""),
        public_api, check_rust,
    ), "R17 missing browser"
    assert release_gate_errors(
        check_all.replace("pg_tests.sh --require-docker", "pg_tests.sh"),
        public_api,
        check_rust,
    ), "R7 required infrastructure"
    assert release_gate_errors(
        check_all.replace("scripts/ci/_provider_environment.sh --self-test", ""),
        public_api,
        check_rust,
    ), "R7 canonical deterministic environment cannot inherit developer opt-outs"
    assert release_gate_errors(
        check_all
        + '\nrun e2e "duplicate-sdk-behavior" npm --prefix e2e run test:sdk-behavior-owners\n',
        public_api,
        check_rust,
    ), "R7 SDK behavior has one deterministic compatibility owner"
    assert release_gate_errors(
        check_all.replace("npm --prefix e2e run test:sdk-latest-canary", ""),
        public_api,
        check_rust,
    ), "R7 latest SDK compatibility"
    assert release_gate_errors(
        check_all.replace("check advisories bans", "check bans"),
        public_api,
        check_rust,
    ), "R7 RustSec advisories cannot disappear behind the bans-only gate"
    assert release_gate_errors(
        check_all.replace("scripts/e2e/k8s_container_e2e.sh", ""),
        public_api,
        check_rust,
    ), "R7 required Kubernetes infrastructure"
    assert release_gate_errors(
        check_all,
        public_api + '\nexcluded="awaken-cli"\n',
        check_rust,
    ), (
        "R7 public API exclusion"
    )
    without_e2e_group = check_all.replace(
        'run e2e "deterministic-e2e"', 'run static "deterministic-e2e"'
    ).replace(
        'run e2e "latest-managed-sdk"', 'run static "latest-managed-sdk"'
    )
    assert release_gate_errors(
        without_e2e_group,
        public_api,
        check_rust,
    ), "R7 independently sharded release group"

    # Kubernetes-control gate decision table: C12 an exact operator digest is
    # admitted; C13 that image enters the shared importer; C14 both live-test
    # gates are set on the one canonical k8s_e2e invocation; C15 absent override
    # builds current source and resolves a manifest (not config) digest; C16 the
    # nested node uses the shared absolute reserve and actual capacity preflight.
    # K1 all true => helper/marker executes. K2 any false => reject orchestration
    # before a skipped test, stale image or host-size-driven eviction can qualify.
    k8s_gate = (ROOT / K8S_CONTAINER_GATE).read_text(encoding="utf-8")
    assert not k8s_container_gate_errors(k8s_gate), "K1 required control roundtrip"
    for mutation in (
        k8s_gate.replace('k3d_validate_image_digest "$CONTROL_IMAGE"', 'true', 1),
        k8s_gate.replace('CONTROL_IMAGE=$(k3d_image_digest "$SESSION_IMAGE")', 'CONTROL_IMAGE=old:tag', 1),
        k8s_gate.replace('build.sh" "$SESSION_IMAGE" ""', 'build.sh" --ensure-hand "$SESSION_IMAGE" ""', 1),
        k8s_gate.replace('k3d_create_cluster "$CLUSTER" 0', 'k3d_create_cluster "$CLUSTER" 0 2', 1),
        k8s_gate.replace(' "$CONTROL_IMAGE"\n', '\n', 1),
        k8s_gate.replace('AWAKEN_K8S_CONTROL_E2E=1', 'AWAKEN_K8S_CONTROL_E2E=0', 1),
    ):
        assert k8s_container_gate_errors(mutation), "K2 false-green control gate"

    # Workflow cause/effect decision table: C14 the candidate SHA is exact;
    # C15 one full-gate invocation owns all groups; C16 PR/main/release enter
    # that same workflow; C17 evidence uploads even on failure; C18 the runner
    # advertises every required substrate. Effects: E14 accept the workflow
    # graph only when C14-C18 all hold; otherwise E15 reject false-green,
    # partial, duplicate, or untraceable execution. Rules W1 all=>E14; W2
    # missing any cause=>E15; W3 a second workflow invokes check-all=>E15.
    # C19 job/step conditions and shell suffixes can hide execution failures;
    # W4 either conditional skip or a modified command => E15, never acceptance.
    workflows = {
        path.relative_to(ROOT).as_posix(): path.read_text(encoding="utf-8")
        for path in (ROOT / ".github" / "workflows").glob("*.yml")
    }
    check_all_text = (ROOT / "scripts/ci/check-all.sh").read_text(encoding="utf-8")
    assert not workflow_gate_errors(workflows), "W1"
    assert not workflow_command_copy_errors(workflows, check_all_text), \
        "W13 the live workflows spell every gate command the way the manifest does"
    # W13 a workflow other than the canonical pair keeps its own spelling of a
    # suite command => reject; the registered online canary => accept; a
    # registered command nothing runs => reject as stale.
    canary = "sdk-drift-canary.yml"
    assert not workflow_command_copy_errors(
        {canary: "run: npm --prefix e2e run test:sdk-latest-canary\n"},
        "run e2e x npm --prefix e2e run test:sdk-latest-canary\n",
    ), "W13 a command the manifest owns, spelled the same way"
    assert workflow_command_copy_errors(
        {canary: "run: npm --prefix e2e run test:sdk-latest\n"},
        "run e2e x npm --prefix e2e run test:sdk-latest-canary\n",
    ), "W13 a workflow may not hold its own spelling of a gate command"
    assert not workflow_command_copy_errors(
        {canary: "run: pnpm --filter @awaken/managed-sdk-oracle check:python:online\n"}, ""
    ), "W13 a registered workflow-only command"
    assert workflow_only_registry_errors({canary: "run: true\n"}), \
        "W13 a registered command no workflow runs is stale"
    assert not workflow_only_registry_errors(workflows), "W13 the live registry is current"
    # W12 lock-owner table: C4 root pnpm importers exclude E2E => the reviewed
    # npm lock is its sole dependency source. A duplicate importer is rejected
    # even if release happens to install via npm today.
    pnpm_lock = (ROOT / "pnpm-lock.yaml").read_text(encoding="utf-8")
    assert not e2e_lock_owner_errors(pnpm_lock), "W12a single E2E lock owner"
    assert e2e_lock_owner_errors(pnpm_lock.replace("importers:\n", "importers:\n  e2e:\n", 1)), "W12b duplicate importer"
    quality = workflows[QUALITY_WORKFLOW]
    # W12 dependency-owner table: C1 quality already installs the exact npm
    # lock; C2 release uses that same lock once; C3 no pnpm E2E install remains.
    # W12a C1+C2+C3 => one dependency tree. W12b !C2 or !C3 => reject a
    # missing or parallel release tree before native Session tests can run.
    release = workflows[RELEASE_WORKFLOW]
    for broken in (
        release.replace("npm ci --prefix e2e", "pnpm -C e2e install --frozen-lockfile", 1),
        release.replace("npm ci --prefix e2e", "true", 1),
        release.replace("npm ci --prefix e2e", "npm ci --prefix e2e\n          npm ci --prefix e2e", 1),
    ):
        changed = dict(workflows)
        changed[RELEASE_WORKFLOW] = broken
        assert any("canonical E2E npm lock" in error for error in workflow_gate_errors(changed)), "W12b"
    # W11 cause/effect table: a portability job is configured; selector is
    # Linux/foreign/absent, invocation count is one/two, job/step is unconditional
    # or skipped. Only Linux + one unconditional call retains real scoped
    # qualification without forcing it on Windows/macOS.
    # Keep the existing full-gate-only projection test independent below.
    call = next(line for line in quality.splitlines()
                if "run: scripts/ci/check-trusted-host-portability.sh" in line)
    for broken in (
        call.replace("'Linux'", "'Windows'"),
        "        run: scripts/ci/check-trusted-host-portability.sh",
        call + "\n" + call,
        "        if: false\n" + call,
    ):
        changed = dict(workflows)
        changed[QUALITY_WORKFLOW] = quality.replace(call, broken, 1)
        assert any("portability matrix" in error for error in workflow_gate_errors(changed)), "W11"
    changed = dict(workflows)
    changed[QUALITY_WORKFLOW] = quality.replace(
        "  trusted_host_portability:\n", "  trusted_host_portability:\n    if: false\n", 1,
    )
    assert any("portability matrix" in error for error in workflow_gate_errors(changed)), "W11 job skip"
    prefix, full_quality = quality.split("  full_quality:\n", 1)
    workflow_mutations = (
        quality.replace('test "$(git rev-parse HEAD)" = "$source_revision"', "true", 1),
        quality.replace("run: scripts/ci/check-all.sh", "run: scripts/ci/check-all.sh --group rust", 1),
        quality.replace("pull_request:", "pull_request_removed:", 1),
        prefix + "  full_quality:\n" + full_quality.replace("if: always()", "if: success()", 1),
        quality.replace("awaken-full-gate", "generic-runner", 1),
        quality.replace("run: scripts/ci/check-all.sh", "run: scripts/ci/check-all.sh || true", 1),
        quality.replace("run: scripts/ci/check-all.sh", "if: false\n        run: scripts/ci/check-all.sh", 1),
        quality.replace("  full_quality:", "  full_quality:\n    if: false", 1),
    )
    for rule, mutated in enumerate(workflow_mutations, 2):
        changed = dict(workflows)
        changed[QUALITY_WORKFLOW] = mutated
        assert workflow_gate_errors(changed), f"W{rule}"
    # E1 complete recorded gate + root-preserving upload/download + original
    # proof consumer => accept. E2 remove record selection/file, nest extraction
    # again, or move the consumer => reject before packaging. These mutations
    # keep the remaining quality workflow intact, preventing token-only green.
    for broken in (
        quality.replace(" --full-gate --record", "", 1),
        quality.replace("            scripts/ci/gate_record.json\n", "", 1),
    ):
        changed = dict(workflows); changed[QUALITY_WORKFLOW] = broken
        assert workflow_gate_errors(changed), "E2 missing executed full-gate record"
    for broken in (
        release.replace("          path: .\n", "          path: target/test-timings\n", 1),
        release.replace("AWAKEN_FORMAL_RUN_EVIDENCE: target/test-timings/formal-run.json", "AWAKEN_FORMAL_RUN_EVIDENCE: nested/formal-run.json", 1),
    ):
        changed = dict(workflows); changed[RELEASE_WORKFLOW] = broken
        assert workflow_gate_errors(changed), "E2 artifact consumer path mismatch"
    # W10 cause/effect table: C1 exact full-quality upload exists; C2 its
    # condition is unconditional-on-result; C3 its artifact binds SHA/attempt
    # and includes both evidence owners; C4 another job also uploads evidence.
    # W10a C1+C2+C3, either C4 => accept. W10b !C1|!C2|!C3 => reject even
    # with C4. Target the owning step, never the first matching token: the
    # portability job must neither mask a broken upload nor make a valid one fail.
    evidence_name = "      - name: Retain exact-SHA quality evidence\n"
    before_evidence, evidence = full_quality.split(evidence_name, 1)
    for condition in ("if: success()", "if: always() && false", "if: false"):
        changed = dict(workflows)
        changed[QUALITY_WORKFLOW] = prefix + "  full_quality:\n" + before_evidence + evidence_name + evidence.replace("if: always()", condition)
        assert workflow_gate_errors(changed), f"W10b {condition}"
    for marker in (evidence_name, "target/test-timings/", "web/test-results/", "${{ github.sha }}", "${{ github.run_attempt }}", "if-no-files-found: error", "actions/upload-artifact@"):
        changed = dict(workflows)
        changed[QUALITY_WORKFLOW] = prefix + "  full_quality:\n" + before_evidence + (evidence_name + evidence).replace(marker, "removed")
        assert workflow_gate_errors(changed), f"W10b missing {marker}"
    changed = dict(workflows)
    changed[QUALITY_WORKFLOW] = prefix.replace("if: always()", "if: success()") + "  full_quality:\n" + full_quality
    assert not workflow_gate_errors(changed), "W10a unrelated job condition"
    changed[QUALITY_WORKFLOW] = "\n".join(quality.splitlines()[:quality.splitlines().index("  trusted_host_portability:")]) + "\n  full_quality:\n" + full_quality
    assert not workflow_gate_errors(changed), "W10a full upload alone"
    duplicate = dict(workflows)
    duplicate[".github/workflows/parallel-quality.yml"] = quality
    assert workflow_gate_errors(duplicate), "W3 duplicate owner"
    assert release_gate_errors(
        check_all.replace("AWAKEN_TEST_TIMINGS_FILE", "REMOVED_TIMINGS_FILE"),
        public_api,
        check_rust,
    ), "R7 timing artifact"
    assert release_gate_errors(
        check_all
        + '\nrun static "duplicate-authority-arithmetic" '
        + 'python3 scripts/ci/check_authority_arithmetic.py\n',
        public_api,
        check_rust,
    ), "R7 authority arithmetic has one Rust-gate owner"
    assert release_gate_errors(
        check_all,
        public_api,
        check_rust.replace(
            "python3 scripts/ci/check_authority_arithmetic.py",
            "python3 scripts/ci/removed_authority_arithmetic.py",
        ),
    ), "R7 authority arithmetic cannot disappear from its Rust-gate owner"
    # Public-API toolchain decision table: C1 exact generator version and C2
    # exact dated rustdoc toolchain. E1 the release manifest is reproducible;
    # E2 either coordinate drifting is rejected before snapshots are trusted.
    # R11 C1+C2 -> E1; R12 !C1 -> E2; R13 !C2 -> E2.
    assert release_gate_errors(
        check_all,
        public_api.replace(
            'PUBLIC_API_TOOL_VERSION="0.52.0"',
            'PUBLIC_API_TOOL_VERSION="0.53.0"',
        ),
        check_rust,
    ), "R12 public API generator drift"
    assert release_gate_errors(
        check_all,
        public_api.replace(
            'PUBLIC_API_NIGHTLY="nightly-2026-07-14"',
            'PUBLIC_API_NIGHTLY="nightly"',
        ),
        check_rust,
    ), "R13 public API nightly drift"

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
