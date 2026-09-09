#!/usr/bin/env python3
"""Audited Kani loop options shared by the existing runner and ledger checker.

These bounds change finite loop expansion, not the input domain or checks. Every
unwinding assertion remains enabled, so a reachable extra iteration fails proof.
Pinned Kani/std v0 identities select audited library loops. All unlisted loops and recursion
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

# Direct resume fixtures retain a real empty serde_json object, empty content/
# context/state Vecs and real Drop. These exact GOTO bounds cover four empty
# tree loops, two Value/ContentBlock recursion points and six empty slice Drop
# loops. Zero element/tree iterations need bound 1 (exit); recursion bound 1
# retains its assertion too. Any deeper actual structure
# fails proof. All unlisted loops retain global 16, including comparisons and
# allocation. CBMC symex_bmc.cpp::get_unwind_recursion consumes --unwindset for
# exact function ids too; the function entries below are not loop wildcards.
TOOL_RESULT_EMPTY_OBJECT_LOOPS = (
    ('_RINvMsj_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree8navigateINtNtB8_4node6HandleINtB11_7NodeRefNtNtB11_6marker5DyingNtNtBc_6string6StringNtNtCsbpSFxJTBWlC_10serde_json5value5ValueNtB1z_4LeafENtB1z_4EdgeE16deallocating_endNtNtBc_5alloc6GlobalEB2h_.0', 1),
    ('_RNvMsn_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree8navigateINtNtB7_4node7NodeRefNtNtB10_6marker5DyingNtNtBb_6string6StringNtNtCsbpSFxJTBWlC_10serde_json5value5ValueNtB1k_14LeafOrInternalE15first_leaf_edgeB22_.0', 1),
    ('_RNvXsy_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree3mapINtB5_8IntoIterNtNtBb_6string6StringNtNtCsbpSFxJTBWlC_10serde_json5value5ValueENtNtNtCsjxLPRqQEPX2_4core3ops4drop4Drop4dropB1w_.0', 1),
    ('_RINvMsj_NtNtNtCs5YUhLFlCshu_5alloc11collections5btree8navigateINtNtB8_4node6HandleINtB11_7NodeRefNtNtB11_6marker5DyingNtNtBc_6string6StringNtNtCsbpSFxJTBWlC_10serde_json5value5ValueNtB1z_4LeafENtB1z_4EdgeE17deallocating_nextNtNtBc_5alloc6GlobalEB2h_.0', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeNtNtCsbpSFxJTBWlC_10serde_json5value5ValueEBK_', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeNtNtNtCsecBtpv2pihN_21awaken_agent_contract5agent7content12ContentBlockEBM_', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeSNtNtNtCsecBtpv2pihN_21awaken_agent_contract5agent7content19SearchResultContentEBN_.0', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeSNtNtNtCsecBtpv2pihN_21awaken_agent_contract5agent7content12ContentBlockEBN_.0', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeSNtNtCs5YUhLFlCshu_5alloc6string6StringECs6MUXvrZ2NgI_3std.0', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeSNtNtNtCsecBtpv2pihN_21awaken_agent_contract5agent5state7CommandEBN_.0', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeSNtNtCsbpSFxJTBWlC_10serde_json5value5ValueEBL_.0', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeSNtNtNtCsecBtpv2pihN_21awaken_agent_contract5agent7message7MessageEBN_.0', 1),
)

# Actual ErasureProgress two-slot GOTOs, pinned nightly/Kani above. These
# bounds retain the complete 0..2-row input list (including duplicates/order),
# and the count proof may append one row. All other loops retain global 32,
# including the 24-byte overflow error comparison and allocation/Drop checks.
ERASURE_PROGRESS_LOOPS = (
    # vec/mod.rs:3999: at most two yielded targets plus loop exit.
    ('_RINvMsj_NtCs5YUhLFlCshu_5alloc3vecINtB6_3VecNtCs7LhrEI5S3s3_31awaken_data_subject_application13ErasureTargetE16extend_desugaredINtNtNtNtCsjxLPRqQEPX2_4core4iter8adapters7flatten7FlattenINtNtNtB2a_5array4iter8IntoIterINtNtB2a_6option6OptionBG_EKj2_EEEBI_.0', 3),
    # flatten.rs:855: the outer iterator owns exactly two Option slots.
    ('_RNvXsI_NtNtNtCsjxLPRqQEPX2_4core4iter8adapters7flattenINtB5_13FlattenCompatINtNtNtBb_5array4iter8IntoIterINtNtBb_6option6OptionNtCs7LhrEI5S3s3_31awaken_data_subject_application13ErasureTargetEKj2_EINtB1I_8IntoIterB21_EENtNtNtB9_6traits8iterator8Iterator4nextB23_.0', 3),
    # slice/iter/macros.rs:331: at most three targets after one append, plus exit.
    ('_RINvXs2J_NtNtCsjxLPRqQEPX2_4core5slice4iterINtB7_4IterNtCs7LhrEI5S3s3_31awaken_data_subject_application13ErasureTargetENtNtNtNtBb_4iter6traits8iterator8Iterator3anyNCNvXsf_NtB9_3cmpBQ_NtB2K_13SliceContains14slice_contains0EBS_.0', 4),
    # slice/cmp.rs:132: full target frame equality covers at most three rows.
    ('_RNvXs2_NtNtCsjxLPRqQEPX2_4core5slice3cmpNtCs7LhrEI5S3s3_31awaken_data_subject_application13ErasureTargetINtB5_14SlicePartialEqBC_E17equal_same_lengthBE_.0', 4),
    # array/mod.rs:936: each arbitrary bool array has exactly two slots.
    ('_RINvNtCsjxLPRqQEPX2_4core5array18try_from_fn_erasedINtNtNtB4_3ops9try_trait17NeverShortCircuitbEQINtNtB2_5drain5DrainuKj2_INtBQ_7WrappedbuNCINvYbNtCsjPMFoRXFuRd_4kani9Arbitrary9any_arrayKB1T_E0EEECs7LhrEI5S3s3_31awaken_data_subject_application.0', 3),
)

# Profiles bind solver options to their exact production package and harnesses.
PROFILES = {
    "resume-validator-empty-object-v1": (
        "awaken-runtime-contract",
        frozenset({"resume_validator_preserves_complete_binding_decision_table"}),
        TOOL_RESULT_EMPTY_OBJECT_LOOPS,
        "cadical",
    ),
    'erasure-progress-selection-v1': (
        "awaken-data-subject-application",
        frozenset({'erasure_progress_selects_exact_first_pending_target', 'erasure_checkpoint_action_requires_all_targets_and_accountability'}),
        ERASURE_PROGRESS_LOOPS + (
            # index_range.rs:167: first-match scan of two adapter references, plus exit.
            ('_RINvXs_NtNtCsjxLPRqQEPX2_4core3ops11index_rangeNtB5_10IndexRangeNtNtNtNtB9_4iter6traits8iterator8Iterator8try_folduNCINvMs6_NtNtNtB9_5array4iter10iter_innerINtB1Y_15PolymorphicIterSINtNtNtB9_3mem12maybe_uninit11MaybeUninitTNtCs7LhrEI5S3s3_31awaken_data_subject_application13ErasureTargetINtNtB9_6option6OptionRhEEEE8try_folduNCINvNvB10_4find5checkB3y_NCINvMs0_B3B_NtB3B_15ErasureProgress14pending_targetB4X_E0E0INtNtB7_12control_flow11ControlFlowB3y_EE0B6B_EB3B_.0', 3),
        ),
        "cadical",  # Explicitly retains Kani's default solver.
    ),
    'erasure-checkpoint-successor-v1': (
        "awaken-data-subject-application",
        frozenset({'erasure_checkpoint_successor_preserves_all_completed_targets'}),
        ERASURE_PROGRESS_LOOPS + (
            # slice/iter/macros.rs:314: all visits at most two old rows plus exit.
            ('_RINvXs2J_NtNtCsjxLPRqQEPX2_4core5slice4iterINtB7_4IterNtCs7LhrEI5S3s3_31awaken_data_subject_application13ErasureTargetENtNtNtNtBb_4iter6traits8iterator8Iterator3allNCNvMs0_BS_NtBS_15ErasureProgress7follows0EBS_.0', 3),
        ),
        "cadical",  # Same default solver; global 32 retains allocation and Drop.
    ),
    'erasure-progress-count-v1': (
        "awaken-data-subject-application",
        frozenset({'erasure_progress_counts_selected_target_once_or_preserves_overflow'}),
        ERASURE_PROGRESS_LOOPS + (
            # index_range.rs:167: the same two-slot scan with unit adapter payloads.
            ('_RINvXs_NtNtCsjxLPRqQEPX2_4core3ops11index_rangeNtB5_10IndexRangeNtNtNtNtB9_4iter6traits8iterator8Iterator8try_folduNCINvMs6_NtNtNtB9_5array4iter10iter_innerINtB1Y_15PolymorphicIterSINtNtNtB9_3mem12maybe_uninit11MaybeUninitTNtCs7LhrEI5S3s3_31awaken_data_subject_application13ErasureTargetINtNtB9_6option6OptionuEEEE8try_folduNCINvNvB10_4find5checkB3y_NCINvMs0_B3B_NtB3B_15ErasureProgress14pending_targetuE0E0INtNtB7_12control_flow11ControlFlowB3y_EE0B6x_EB3B_.0', 3),
            # slice/iter/macros.rs:276: occurrence fold visits at most three target rows.
            ('_RINvXs2J_NtNtCsjxLPRqQEPX2_4core5slice4iterINtB7_4IterNtCs7LhrEI5S3s3_31awaken_data_subject_application13ErasureTargetENtNtNtNtBb_4iter6traits8iterator8Iterator4foldjNCINvNtNtB1Z_8adapters3map8map_foldRBQ_jjNCINvNvXs1_NtB2L_6filterINtB3u_6FilterppEB1T_5count8to_usizeB3d_NCNvNtBS_20erasure_verification20check_selected_count0E0NCINvXsK_NtB1X_5accumjNtB5o_3Sum3sumINtB2J_3MapBF_B3j_EE0E0EBS_.0', 4),
        ),
        "cadical",  # Explicitly retains Kani's default solver.
    ),
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
