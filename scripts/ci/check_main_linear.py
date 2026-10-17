#!/usr/bin/env python3
"""Keep `main` linear: every update is a fast-forward that adds no merge commit.

Runs as the `reference-transaction` hook (lefthook) in the `prepared` state, so a
violating update is refused before any ref moves, whichever command moved it
(`merge`, `push`, `update-ref`, `branch -f`, `reset`). A deliberate, reviewed
history rewrite sets AWAKEN_ALLOW_MAIN_REWRITE=1 for that one command.

Decision table (--self-test):
| Rule | update of refs/heads/main              | result  |
| L1   | another ref                            | ignored |
| L2   | creation (old is zero, main absent)    | allowed |
| L10  | old not stated (zero) while main exists| judged against main's current value |
| L3   | deletion (new is zero)                 | refused |
| L11  | `pack-refs` pruning the loose copy: new is zero, no `packed-refs.lock` is held and `packed-refs` already records main at old | allowed (main keeps its value) |
| L4   | new does not descend from old          | refused |
| L5   | fast-forward adding a merge commit     | refused |
| L6   | fast-forward of single-parent commits  | allowed |
| L7   | refused case with the explicit override| allowed |
| L8   | state `preparing` / `committed` / `aborted` | ignored |
| L9   | any other argument                     | usage error |
"""
from __future__ import annotations

import os
import subprocess
import sys
from collections.abc import Callable

ZERO = "0" * 40
MAIN = "refs/heads/main"


def git(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], capture_output=True, text=True)


def is_ancestor(old: str, new: str) -> bool:
    return git("merge-base", "--is-ancestor", old, new).returncode == 0


def merges_between(old: str, new: str) -> list[str]:
    return git("rev-list", "--merges", f"{old}..{new}").stdout.split()


def current_main() -> str:
    """The value main holds while the transaction is prepared (or zero if absent)."""
    out = git("rev-parse", "--verify", "--quiet", MAIN).stdout.strip()
    return out or ZERO


def packed_main() -> str | None:
    """main's value in `packed-refs` when no packed transaction holds the lock, else None.

    `pack-refs` (also run by `gc --auto`) first writes main into `packed-refs`, releases the
    lock, then deletes the loose copy, which the hook sees as `old -> zero`. A real deletion
    always holds `packed-refs.lock` while it reports `old -> zero`.
    """
    common = git("rev-parse", "--git-common-dir").stdout.strip()
    if not common or os.path.exists(os.path.join(common, "packed-refs.lock")):
        return None
    try:
        with open(os.path.join(common, "packed-refs"), encoding="utf-8") as packed:
            for line in packed:
                parts = line.split()
                if len(parts) == 2 and parts[1] == MAIN:
                    return parts[0]
    except FileNotFoundError:
        pass
    return None


def violations(lines: list[str], ancestor: Callable[[str, str], bool],
               merges: Callable[[str, str], list[str]], current: Callable[[], str],
               packed: Callable[[], str | None] = lambda: None) -> list[str]:
    found = []
    for line in lines:
        parts = line.split()
        if len(parts) != 3 or parts[2] != MAIN:
            continue
        old, new, _ = parts
        if old == ZERO:
            # A caller that does not state the expected old value (`update-ref main X`,
            # `branch -f`) is reported with a zero old value; judge it against the value
            # main still holds while the transaction is prepared.
            old = current()
        if old == ZERO:
            continue
        if new == ZERO:
            if packed() != old:
                found.append("main may not be deleted")
        elif not ancestor(old, new):
            found.append(f"main {old[:10]} -> {new[:10]} is not a fast-forward; rebase onto main instead")
        else:
            added = merges(old, new)
            if added:
                found.append(f"main {old[:10]} -> {new[:10]} adds merge commit(s) {', '.join(m[:10] for m in added)}; "
                             "rebase so main stays linear")
    return found


def main(argv: list[str]) -> int:
    states = ("preparing", "prepared", "committed", "aborted")
    if len(argv) != 2 or argv[1] not in (*states, "--self-test"):
        print(f"usage: {argv[0]} {{{'|'.join(states)}|--self-test}}", file=sys.stderr)
        return 2
    if argv[1] == "--self-test":
        self_test()
        return 0
    if argv[1] != "prepared":
        return 0
    errors = violations(sys.stdin.read().splitlines(), is_ancestor, merges_between, current_main, packed_main)
    if errors and os.environ.get("AWAKEN_ALLOW_MAIN_REWRITE") != "1":
        for error in errors:
            print(f"ERROR: {error} (deliberate rewrite: AWAKEN_ALLOW_MAIN_REWRITE=1)", file=sys.stderr)
        return 1
    return 0


def self_test() -> None:
    a, b, c = "a" * 40, "b" * 40, "c" * 40
    anc = {(a, b), (a, c)}
    ancestor = lambda o, n: (o, n) in anc
    merges = lambda o, n: ["m" * 40] if n == c else []
    absent = lambda: ZERO
    assert violations([f"{a} {b} refs/heads/feature"], ancestor, merges, absent) == [], "L1"
    assert violations([f"{ZERO} {b} {MAIN}"], ancestor, merges, absent) == [], "L2"
    assert "deleted" in violations([f"{a} {ZERO} {MAIN}"], ancestor, merges, absent)[0], "L3"
    assert "deleted" in violations([f"{ZERO} {ZERO} {MAIN}"], ancestor, merges, lambda: a)[0], "L3 unstated old"
    assert "deleted" in violations([f"{a} {ZERO} {MAIN}"], ancestor, merges, absent, lambda: None)[0], \
        "L3 packed-refs locked or not recording main"
    assert "deleted" in violations([f"{a} {ZERO} {MAIN}"], ancestor, merges, absent, lambda: b)[0], \
        "L3 packed-refs records another value"
    assert violations([f"{a} {ZERO} {MAIN}"], ancestor, merges, absent, lambda: a) == [], "L11"
    assert "fast-forward" in violations([f"{b} {a} {MAIN}"], ancestor, merges, absent)[0], "L4"
    assert "merge commit" in violations([f"{a} {c} {MAIN}"], ancestor, merges, absent)[0], "L5"
    assert violations([f"{a} {b} {MAIN}"], ancestor, merges, absent) == [], "L6"
    assert "fast-forward" in violations([f"{ZERO} {a} {MAIN}"], ancestor, merges, lambda: b)[0], \
        "L10 an unstated old value is judged against main's current value"
    assert violations([f"{ZERO} {b} {MAIN}"], ancestor, merges, lambda: a) == [], "L10 forward from current"
    os.environ["AWAKEN_ALLOW_MAIN_REWRITE"] = "1"
    try:
        assert main(["x", "committed"]) == 0, "L8"
        assert main(["x", "preparing"]) == 0, "L8 Git 2.48+ announces the transaction first"
        assert main(["x", "--unknown"]) == 2, "L9 unknown argument is refused"
    finally:
        os.environ.pop("AWAKEN_ALLOW_MAIN_REWRITE")
    print("check_main_linear self-test: the decision table holds")


if __name__ == "__main__":
    sys.exit(main(sys.argv))
