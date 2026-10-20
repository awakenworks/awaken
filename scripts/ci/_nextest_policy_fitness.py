"""Nextest runner policy and its decision-table regressions."""

from __future__ import annotations

import re
import shlex
import tomllib
from pathlib import Path

NEXTEST_CONFIG = ".config/nextest.toml"
# A unit or integration test that has not finished in five minutes is not slow,
# it is stuck. The bound exists so no configuration edit can restore the
# unbounded run that once parked a gate for eight hours.
MAX_TEST_DEADLINE_SECONDS = 300.0
DURATION = re.compile(r"(?P<value>\d+(?:\.\d+)?)\s*(?P<unit>ms|s|m|h)\Z")
DURATION_UNITS = {"ms": 0.001, "s": 1.0, "m": 60.0, "h": 3600.0}

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


def unbounded_workspace_test(line: str) -> bool:
    """Distinguish Cargo build modes from libtest arguments and other commands."""
    lexer = shlex.shlex(line.rstrip().removesuffix("\\"), posix=True, punctuation_chars=";&|")
    lexer.whitespace_split = True
    try:
        tokens = list(lexer)
    except ValueError:
        return bool(re.search(r"cargo test --workspace", line))
    for index in range(len(tokens) - 2):
        if tokens[index:index + 3] != ["cargo", "test", "--workspace"]:
            continue
        options = []
        for token in tokens[index + 3:]:
            if token == "--" or set(token) <= set(";&|"):
                break
            options.append(token)
        if not {"--no-run", "--doc"}.intersection(options):
            return True
    return False


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

    def check_leak_timeout(value: object, location: str) -> None:
        period = value.get("period", "") if isinstance(value, dict) else ""
        match = DURATION.fullmatch(str(period).strip())
        seconds = float(match["value"]) * DURATION_UNITS[match["unit"]] if match else None
        if (not isinstance(value, dict) or value.get("result") != "fail"
                or seconds is None or not 0 < seconds <= 0.1):
            errors.append(
                f"{NEXTEST_CONFIG} {location} must fail process leaks within 100ms; "
                'set leak-timeout = { period = "100ms", result = "fail" }'
            )

    check_leak_timeout(default.get("leak-timeout"), "[profile.default]")
    for name, profile in (profiles.items() if isinstance(profiles, dict) else ()):
        if name != "default" and "leak-timeout" in profile:
            check_leak_timeout(profile["leak-timeout"], f"[profile.{name}]")
        for index, override in enumerate(profile.get("overrides", []) or []):
            if "leak-timeout" in override:
                check_leak_timeout(override["leak-timeout"], f"[profile.{name}] override {index}")
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
        if unbounded_workspace_test(line)
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


def self_test(root: Path) -> None:
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
    deadline_config = (root / NEXTEST_CONFIG).read_text(encoding="utf-8")
    deadline_gate = (root / "scripts/ci/check-rust.sh").read_text(encoding="utf-8")
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
    for suffix, unbounded in (
        ("--locked --no-run --timings", False), ('"--no-run"', False),
        ("--no-run \\", False), ("--locked \\", True),
        ("--locked", True), ("-- --no-run", True),
        ("; echo --no-run", True), ("# --no-run", True),
        ("--test=contains--no-run", True), ("--no-run=false", True),
        ("--no-run; cargo test --workspace", True),
        ("--no-run && cargo test --workspace", True),
        ("-- --doc", True), ("--locked 'unterminated", True),
    ):
        errors = test_deadline_errors(
            deadline_config, deadline_gate + f"\ncargo test --workspace {suffix}\n"
        )
        assert bool(errors) == unbounded, f"D6 Cargo execution mode: {suffix}"
    assert 'failure-output = "immediate-final"' in deadline_config, "D8 fixture"
    for lost in ('failure-output = "immediate"', 'failure-output = "final"', ""):
        assert test_deadline_errors(
            deadline_config.replace('failure-output = "immediate-final"', lost),
            deadline_gate,
        ), f"D8 failure output {lost!r} does not survive the gate filter"
    # Leak decision table: L1 explicit fail within 100ms -> accept; L2 missing,
    # scalar, success result or invalid/widened period -> reject; L3 a profile
    # or per-test override cannot restore successful leaks or widen detection.
    leak = 'leak-timeout = { period = "100ms", result = "fail" }'
    assert leak in deadline_config, "L1 fixture"
    for replacement in ("", 'leak-timeout = "100ms"',
                        'leak-timeout = { period = "100ms", result = "pass" }',
                        'leak-timeout = { period = "1s", result = "fail" }',
                        'leak-timeout = { period = "0ms", result = "fail" }',
                        'leak-timeout = { period = "invalid", result = "fail" }'):
        assert test_deadline_errors(deadline_config.replace(leak, replacement), deadline_gate), "L2"
    for section in ('[profile.ci]', "[[profile.default.overrides]]\nfilter = 'test(leaky)'",
                    "[[profile.ci.overrides]]\nfilter = 'test(leaky)'"):
        for value in ('"100ms"', '{ period = "100ms", result = "pass" }',
                      '{ period = "1s", result = "fail" }'):
            assert test_deadline_errors(
                deadline_config + f"\n{section}\nleak-timeout = {value}\n", deadline_gate
            ), "L3 weakening override"
        assert not test_deadline_errors(
            deadline_config + f"\n{section}\n{leak}\n", deadline_gate
        ), "L1 equivalent override"
