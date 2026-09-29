#!/usr/bin/env python3
"""The formal gate's proof budgets, and the one direction they may move.

A Kani batch that does not converge has two possible remedies: reduce what the
proof costs, or give it longer. Only the first one ends. Raising the deadline
looks like progress because the run gets further each time, so it is the remedy
that gets reached for -- this repository has reached for it, from 30 minutes to
60 to 120, and the same batch timed out at every setting because the cost tracks
the free symbolic domain rather than the clock.

So the deadline is declared here and an environment override may only *lower*
it. A run that wants longer than the declared ceiling is refused, and the refusal
names the two remedies that actually terminate. Raising the ceiling is still
possible -- by editing the constant below, which is a reviewable diff that states
who decided a proof deserves more machine time, rather than an env var nobody
sees in the receipt.

Lowering stays free: a fast local loop, a smoke run, or a CI lane that only needs
the cheap harnesses can all ask for less without touching this file.
"""

from __future__ import annotations

import argparse
import sys

# This module reads no repository files; its inputs are its own source and the
# environment it is handed. Declared so `generate_hooks.py` can render any hook
# that comes to invoke it.
INPUT_GLOBS: tuple[str, ...] = ("scripts/ci/formal_budget.py",)

# The declared ceiling for one `cargo kani` batch. 1800s is what the gate has
# historically completed within; the batches that exceed it exceed 7200s too, so
# a larger number buys a longer wait rather than a proof.
#
# Two 2026-09-28 full-gate runs at 3600s and 7200s stalled at the same
# `model_routing::kani_proofs::fallback_selection_preserves_the_complete_publication_pin`
# harness. A subsequent 240s isolated replay also stalled while expanding
# ModelBinding/String admission inside a selector proof. Explicit unwind 16
# still timed out; simply raising or partitioning this batch was not a fix.
# The production selector is now payload-transparent and the proof calls that
# same kernel with stack values. A focused replay finished 15/15 checks, with
# 0.05s solver time; the real ModelCandidate instantiation has a separate unit
# witness. The final-SHA full gate remains the release receipt authority.
KANI_BATCH_TIMEOUT_CEILING_SECONDS = 1800

BUDGETS = {
    "kani-batch-timeout-seconds": (
        "AWAKEN_KANI_BATCH_TIMEOUT_SECONDS",
        KANI_BATCH_TIMEOUT_CEILING_SECONDS,
    ),
}


class BudgetRefused(ValueError):
    """An override asked for more machine time than the declared ceiling."""


def resolve(raw: str | None, ceiling: int, variable: str) -> int:
    """The budget one run may use.

    Causes: C1 no override; C2 an override at or below the ceiling; C3 an
    override above it; C4 an override that is not a positive integer.
    Effects: E1 the ceiling; E2 the override; E3 refusal naming the remedies;
    E4 refusal naming the malformed value.
    Rules: C1 -> E1; C2 -> E2; C3 -> E3; C4 -> E4.
    """

    if raw is None or raw == "":
        return ceiling
    if not raw.isdigit() or int(raw) == 0:
        raise BudgetRefused(
            f"{variable} must be a positive whole number of seconds, not {raw!r}"
        )
    requested = int(raw)
    if requested > ceiling:
        raise BudgetRefused(
            f"{variable}={requested} exceeds the declared ceiling of {ceiling}s. "
            "A batch that needs longer has not been made cheaper: split it, or bound "
            "its proof inputs. If a proof has earned more machine time, raise "
            "KANI_BATCH_TIMEOUT_CEILING_SECONDS in scripts/ci/formal_budget.py so the "
            "decision is reviewable."
        )
    return requested


def selftest() -> None:
    """The decision table above, proven over the boundary and both refusals."""

    ceiling = 100
    var = "AWAKEN_TEST_BUDGET"
    assert resolve(None, ceiling, var) == ceiling, "C1 -> E1 absent"
    assert resolve("", ceiling, var) == ceiling, "C1 -> E1 empty"
    assert resolve("1", ceiling, var) == 1, "C2 -> E2 far below"
    assert resolve("100", ceiling, var) == ceiling, "C2 -> E2 at the boundary"
    for raw in ("101", "10000"):
        try:
            resolve(raw, ceiling, var)
        except BudgetRefused as error:
            assert "exceeds the declared ceiling" in str(error), "C3 -> E3 names the cause"
            assert "split it" in str(error), "C3 -> E3 names a remedy that terminates"
        else:  # pragma: no cover - the assertion below states the failure
            raise AssertionError(f"C3 -> E3 did not refuse {raw}")
    for raw in ("0", "-5", "abc", "60s", "1_000"):
        try:
            resolve(raw, ceiling, var)
        except BudgetRefused as error:
            assert "positive whole number" in str(error), f"C4 -> E4 for {raw}"
        else:  # pragma: no cover
            raise AssertionError(f"C4 -> E4 did not refuse {raw}")
    # The shipped ceiling is the one the gate documents, so a silent edit to the
    # constant cannot pass this test while the docstring still claims 1800s.
    assert KANI_BATCH_TIMEOUT_CEILING_SECONDS == 1800, "declared ceiling is 1800s"
    print("formal_budget self-test: overrides may only lower a declared budget")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument("budget", nargs="?", choices=sorted(BUDGETS), help="budget to resolve")
    parser.add_argument("--self-test", action="store_true", help="run the decision table")
    args = parser.parse_args(argv)

    if args.self_test:
        selftest()
        return 0
    if args.budget is None:
        parser.error("name a budget to resolve, or pass --self-test")

    import os

    variable, ceiling = BUDGETS[args.budget]
    try:
        print(resolve(os.environ.get(variable), ceiling, variable))
    except BudgetRefused as error:
        print(f"formal_budget: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
