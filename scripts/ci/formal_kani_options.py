#!/usr/bin/env python3
"""Audited Kani loop options shared by the existing runner and ledger checker.

These bounds change finite loop expansion, not the input domain or checks. Every
unwinding assertion remains enabled, so a reachable extra iteration fails proof.
Pinned Kani/std v0 identities select audited library loops. Other loops and Drop
retain each harness's global bound; optional solver choices are fixed per profile.
"""

import re
import sys


# Pinned nightly-2026-05-01 / Kani 0.67 std loop identities. No shell expansion.
ACTIVITY_HEIGHT_LOOPS = (
    # navigate.rs:465
    ('_RINvMsj_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree8navigateINtNtB8_4node6HandleINtB11_7NodeRefNtNtB11_6marker5DyingyNtNtB8_7set_val9SetValZSTNtB1z_4LeafENtB1z_4EdgeE17deallocating_nextNtNtBc_5alloc6GlobalECseln4KG8PkwO_23awaken_session_contract.0', 1),
    # search.rs:57
    ('_RINvMs_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree6searchINtNtB7_4node7NodeRefNtNtBY_6marker3MutyNtNtB7_7set_val9SetValZSTNtB1i_14LeafOrInternalE11search_treeyECseln4KG8PkwO_23awaken_session_contract.0', 1),
    # node.rs:558
    ('_RINvMsp_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree4nodeINtB6_7NodeRefNtNtB6_6marker3MutyNtNtB8_7set_val9SetValZSTNtB1a_8InternalE30correct_childrens_parent_linksINtNtNtCsjxLPRqQEPX2_4core3ops5range5RangejEECseln4KG8PkwO_23awaken_session_contract.0', 1),
    # navigate.rs:518
    ('_RINvMsj_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree8navigateINtNtB8_4node6HandleINtB11_7NodeRefNtNtB11_6marker5DyingyNtNtB8_7set_val9SetValZSTNtB1z_4LeafENtB1z_4EdgeE16deallocating_endNtNtBc_5alloc6GlobalECseln4KG8PkwO_23awaken_session_contract.0', 1),
    # node.rs:558
    ('_RINvMsp_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree4nodeINtB6_7NodeRefNtNtB6_6marker3MutyNtNtB8_7set_val9SetValZSTNtB1a_8InternalE30correct_childrens_parent_linksINtNtNtCsjxLPRqQEPX2_4core3ops5range14RangeInclusivejEECseln4KG8PkwO_23awaken_session_contract.0', 1),
    # node.rs:1072
    ('_RINvMsN_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree4nodeINtB6_6HandleINtB6_7NodeRefNtNtB6_6marker3MutyNtNtB8_7set_val9SetValZSTNtB1n_4LeafENtB1n_4EdgeE16insert_recursingNtNtBc_5alloc6GlobalNCNvMs6_NtNtB8_3map5entryINtB3b_11VacantEntryyB1E_E12insert_entry0ECseln4KG8PkwO_23awaken_session_contract.0', 1),
    # navigate.rs:634
    ('_RNvMsn_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree8navigateINtNtB7_4node7NodeRefNtNtB10_6marker5DyingyNtNtB7_7set_val9SetValZSTNtB1k_14LeafOrInternalE15first_leaf_edgeCseln4KG8PkwO_23awaken_session_contract.0', 1),
    # search.rs:57
    ('_RINvMs_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree6searchINtNtB7_4node7NodeRefNtNtBY_6marker5ImmutyNtNtB7_7set_val9SetValZSTNtB1i_14LeafOrInternalE11search_treeyECseln4KG8PkwO_23awaken_session_contract.0', 1),
)

# One pinned UTF-8 loop catalog for the canonical fixed-width encoder. The
# safe String conversion remains part of both proofs, including unwind checks.
FINGERPRINT_ENCODING_LOOPS = (
    "_RNvNtNtCsjxLPRqQEPX2_4core3str11validations19run_utf8_validation.0",
    "_RNvNtNtCsjxLPRqQEPX2_4core3str11validations19run_utf8_validation.1",
    "_RNvNtNtCsjxLPRqQEPX2_4core3str11validations19run_utf8_validation.2",
    "_RNvNvNtCsjxLPRqQEPX2_4core3ptr12align_offset7mod_invCscYh4lW7VexR_21awaken_agent_contract.0",
)

# Profiles bind solver options to their exact production package and harnesses.
PROFILES = {
    "fingerprint-fnv-hex-v1": (
        "awaken-agent-contract",
        frozenset({"fnv_hex_output_preserves_every_u64"}),
        # 24 ASCII wire bytes: at most seven alignment bytes, one 16-byte
        # block, eight tail bytes; bounds include exit. mod_inv needs <=4
        # iterations on the pinned 64-bit target. Global unwind remains 18.
        tuple(zip(FINGERPRINT_ENCODING_LOOPS, (2, 9, 9, 5))),
        "z3",
    ),
    "fingerprint-sha256-hex-v1": (
        "awaken-agent-contract",
        frozenset({"sha256_hex_output_preserves_every_digest_byte"}),
        # 71 ASCII wire bytes: <=7 alignment bytes and four 16-byte blocks;
        # tail 17 is conservative, outer 9 includes exit. Global unwind 66
        # retains every one of the 256 unconstrained input bits. CaDiCaL is
        # independently qualified; the Z3 timeout is not a proof result.
        tuple(zip(FINGERPRINT_ENCODING_LOOPS, (5, 17, 9, 5))),
        "cadical",
    ),
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
        None,
    ),
    "activity-terminal-height-v1": (
        "awaken-session-contract",
        frozenset({"activity_terminal_transition_clears_real_members"}),
        ACTIVITY_HEIGHT_LOOPS[:7],
        None,
    ),
    "retained-accepted-two-replay-v1": (
        "awaken-session-contract",
        frozenset({"deleted_admission_one_retained_accepted_two_27"}),
        (
            ("memcmp.0", 2),
            # core/ptr/mod.rs:1452: 24-byte String swaps three 8-byte chunks;
            # bound 4 includes the loop exit. Global unwind 3 stays unchanged.
            ("_RINvNvNtCsjxLPRqQEPX2_4core3ptr25swap_nonoverlapping_bytes26swap_nonoverlapping_chunksKj8_ECs6MUXvrZ2NgI_3std.0", 4),
        ),
        "z3",
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
    expected_package, allowed, _, _ = profile
    names = _harnesses(options)
    if package != expected_package or not set(names) <= allowed:
        raise ValueError("checked Kani profile package/harness binding mismatch")


def _serialize(profile: tuple, selected: list[str]) -> list[str]:
    # Both invocation expansion and qualification compare this exact argv.
    # The profile owns optional solver selection; callers supply harnesses only.
    _, _, loops, solver = profile
    solver_options = ["--solver", solver] if solver is not None else []
    return ["-Z", "unstable-options", *solver_options, *selected,
            "--cbmc-args", "--unwindset",
            ",".join(f"{name}:{bound}" for name, bound in loops)]


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
    return _serialize(profile, selected)


def checked_loop_override(package: str, prefix: list[str], tail: list[str], unstable: bool) -> bool:
    if not unstable or len(tail) != 3 or tail[:2] != ["--cbmc-args", "--unwindset"]:
        return False
    for profile in PROFILES.values():
        selected = prefix[4 if profile[3] is not None else 2:]
        try:
            _bind(package, selected, profile)
        except ValueError:
            continue
        if prefix + tail == _serialize(profile, selected):
            return True
    # A fixed-solver profile is a newly reserved harness invocation. Removing
    # its other loops or solver must not fall through to the older memcmp-only
    # path. Existing Activity profiles and legacy memcmp calls keep their argv.
    if any(solver is not None and not allowed.isdisjoint(prefix)
           for _, allowed, _, solver in PROFILES.values()):
        return False
    return re.fullmatch(r"memcmp\.0:[1-9][0-9]*", tail[2]) is not None


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
