#!/usr/bin/env python3
"""Audited Kani loop options shared by the existing runner and ledger checker.

These bounds change finite loop expansion, not the input domain or checks. Every
unwinding assertion remains enabled, so a reachable extra iteration fails proof.
The pinned Kani/std v0 identities select height/ancestor loops of the real u64
BTreeSet. Leaf scans, array iteration and Drop retain each harness's global bound.
"""

import re
import sys


# Pinned nightly-2026-05-01 / Kani 0.67 std loop identities. No shell expansion.
ACTIVITY_HEIGHT_LOOPS = (
    # navigate.rs:465
    '_RINvMsj_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree8navigateINtNtB8_4node6HandleINtB11_7NodeRefNtNtB11_6marker5DyingyNtNtB8_7set_val9SetValZSTNtB1z_4LeafENtB1z_4EdgeE17deallocating_nextNtNtBc_5alloc6GlobalECs9zR91oUBBIR_23awaken_session_contract.0',
    # search.rs:57
    '_RINvMs_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree6searchINtNtB7_4node7NodeRefNtNtBY_6marker3MutyNtNtB7_7set_val9SetValZSTNtB1i_14LeafOrInternalE11search_treeyECs9zR91oUBBIR_23awaken_session_contract.0',
    # node.rs:558
    '_RINvMsp_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree4nodeINtB6_7NodeRefNtNtB6_6marker3MutyNtNtB8_7set_val9SetValZSTNtB1a_8InternalE30correct_childrens_parent_linksINtNtNtCsjxLPRqQEPX2_4core3ops5range5RangejEECs9zR91oUBBIR_23awaken_session_contract.0',
    # navigate.rs:518
    '_RINvMsj_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree8navigateINtNtB8_4node6HandleINtB11_7NodeRefNtNtB11_6marker5DyingyNtNtB8_7set_val9SetValZSTNtB1z_4LeafENtB1z_4EdgeE16deallocating_endNtNtBc_5alloc6GlobalECs9zR91oUBBIR_23awaken_session_contract.0',
    # node.rs:558
    '_RINvMsp_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree4nodeINtB6_7NodeRefNtNtB6_6marker3MutyNtNtB8_7set_val9SetValZSTNtB1a_8InternalE30correct_childrens_parent_linksINtNtNtCsjxLPRqQEPX2_4core3ops5range14RangeInclusivejEECs9zR91oUBBIR_23awaken_session_contract.0',
    # node.rs:1072
    '_RINvMsN_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree4nodeINtB6_6HandleINtB6_7NodeRefNtNtB6_6marker3MutyNtNtB8_7set_val9SetValZSTNtB1n_4LeafENtB1n_4EdgeE16insert_recursingNtNtBc_5alloc6GlobalNCNvMs6_NtNtB8_3map5entryINtB3b_11VacantEntryyB1E_E12insert_entry0ECs9zR91oUBBIR_23awaken_session_contract.0',
    # navigate.rs:634
    '_RNvMsn_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree8navigateINtNtB7_4node7NodeRefNtNtB10_6marker5DyingyNtNtB7_7set_val9SetValZSTNtB1k_14LeafOrInternalE15first_leaf_edgeCs9zR91oUBBIR_23awaken_session_contract.0',
    # search.rs:57
    '_RINvMs_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree6searchINtNtB7_4node7NodeRefNtNtBY_6marker5ImmutyNtNtB7_7set_val9SetValZSTNtB1i_14LeafOrInternalE11search_treeyECs9zR91oUBBIR_23awaken_session_contract.0',
)

# Profiles bind solver options to their exact production package and harnesses.
PROFILES = {
    "activity-admission-height-v1": (
        "awaken-session-contract",
        frozenset({
            "activity_next_epoch_inserts_exact_checked_successor",
            "activity_exact_epoch_inserts_only_fresh_positive_member",
            "activity_next_next_admissions_preserve_exact_members",
            "activity_next_exact_admissions_preserve_exact_members",
            "activity_exact_next_admissions_preserve_exact_members",
            "activity_exact_exact_admissions_preserve_exact_members",
        }),
        ACTIVITY_HEIGHT_LOOPS,
    ),
    "activity-terminal-height-v1": (
        "awaken-session-contract",
        frozenset({"activity_terminal_transition_clears_real_members"}),
        ACTIVITY_HEIGHT_LOOPS[:7],
    ),
}


def _harnesses(options: list[str]) -> list[str]:
    if not options or len(options) % 2 or any(
        options[index] != "--harness" for index in range(0, len(options), 2)
    ):
        raise ValueError("checked Kani profile permits only literal harness selections")
    names = options[1::2]
    if len(names) != len(set(names)):
        raise ValueError("checked Kani profile rejects duplicate harnesses")
    return names


def _bind(package: str, options: list[str], profile: tuple) -> None:
    expected_package, allowed, _ = profile
    names = _harnesses(options)
    if package != expected_package or not set(names) <= allowed:
        raise ValueError("checked Kani profile package/harness binding mismatch")


def expand_options(package: str, options: list[str]) -> list[str]:
    if "--checked-loop-profile" not in options:
        return options
    if len(options) < 4 or options[0] != "--checked-loop-profile":
        raise ValueError("checked Kani profile must be the first and sole option owner")
    profile = PROFILES.get(options[1])
    if profile is None:
        raise ValueError("unknown checked Kani profile")
    selected = options[2:]
    _bind(package, selected, profile)
    return ["-Z", "unstable-options", *selected, "--cbmc-args", "--unwindset",
            ",".join(name + ":1" for name in profile[2])]


def checked_loop_override(package: str, prefix: list[str], tail: list[str], unstable: bool) -> bool:
    if not unstable or len(tail) != 3 or tail[1] != "--unwindset":
        return False
    if re.fullmatch(r"memcmp\.0:[1-9][0-9]*", tail[2]):
        return True  # Existing strict memcmp-only invocation contract.
    if prefix[:2] != ["-Z", "unstable-options"]:
        return False
    for profile in PROFILES.values():
        try:
            _bind(package, prefix[2:], profile)
        except ValueError:
            continue
        if tail[2] == ",".join(name + ":1" for name in profile[2]):
            return True
    return False


def main() -> int:
    try:
        if len(sys.argv) < 3:
            raise ValueError("checked Kani options require package and selections")
        options = expand_options(sys.argv[1], sys.argv[2:])
        for option in options:
            print(option)
        return 0
    except ValueError as error:
        print(f"formal Kani options: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
