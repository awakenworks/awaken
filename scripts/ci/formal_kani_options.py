#!/usr/bin/env python3
"""Audited Kani solver and loop profiles: the one owner of non-default Kani options.

A harness opts into a profile with its own `/// @kani profile=<name>` tag; the
derived plan (``formal_plan.py``) serializes the profile into the batch argv.
Loop bounds change finite loop expansion, not the input domain or checks. Every
unwinding assertion remains enabled, so a reachable extra iteration fails proof.
Pinned Kani/std v0 identities select audited library loops. All unlisted loops
and recursion retain each harness's global bound; optional solver choices are
fixed per profile.
"""

from __future__ import annotations

from dataclasses import dataclass


# Pinned nightly-2026-05-01 / Kani 0.67 std loop identities. No shell expansion.
ACTIVITY_HEIGHT_LOOPS = (
    # navigate.rs:465
    ('_RINvMsj_NtNtNtCs6NSMbdZCE7r_5alloc11collections5btree8navigateINtNtB8_4node6HandleINtB11_7NodeRefNtNtB11_6marker5DyingyNtNtB8_7set_val9SetValZSTNtB1z_4LeafENtB1z_4EdgeE17deallocating_nextNtNtBc_5alloc6GlobalECsaNb1NCWtnRw_29awaken_session_state_contract.0', 1),
    # search.rs:57
    ('_RINvMs_NtNtNtCs6NSMbdZCE7r_5alloc11collections5btree6searchINtNtB7_4node7NodeRefNtNtBY_6marker3MutyNtNtB7_7set_val9SetValZSTNtB1i_14LeafOrInternalE11search_treeyECsaNb1NCWtnRw_29awaken_session_state_contract.0', 1),
    # node.rs:558
    ('_RINvMsp_NtNtNtCs6NSMbdZCE7r_5alloc11collections5btree4nodeINtB6_7NodeRefNtNtB6_6marker3MutyNtNtB8_7set_val9SetValZSTNtB1a_8InternalE30correct_childrens_parent_linksINtNtNtCsci0VKyKEi6N_4core3ops5range5RangejEECsaNb1NCWtnRw_29awaken_session_state_contract.0', 1),
    # navigate.rs:518
    ('_RINvMsj_NtNtNtCs6NSMbdZCE7r_5alloc11collections5btree8navigateINtNtB8_4node6HandleINtB11_7NodeRefNtNtB11_6marker5DyingyNtNtB8_7set_val9SetValZSTNtB1z_4LeafENtB1z_4EdgeE16deallocating_endNtNtBc_5alloc6GlobalECsaNb1NCWtnRw_29awaken_session_state_contract.0', 1),
    # node.rs:558
    ('_RINvMsp_NtNtNtCs6NSMbdZCE7r_5alloc11collections5btree4nodeINtB6_7NodeRefNtNtB6_6marker3MutyNtNtB8_7set_val9SetValZSTNtB1a_8InternalE30correct_childrens_parent_linksINtNtNtCsci0VKyKEi6N_4core3ops5range14RangeInclusivejEECsaNb1NCWtnRw_29awaken_session_state_contract.0', 1),
    # node.rs:1072
    ('_RINvMsN_NtNtNtCs6NSMbdZCE7r_5alloc11collections5btree4nodeINtB6_6HandleINtB6_7NodeRefNtNtB6_6marker3MutyNtNtB8_7set_val9SetValZSTNtB1n_4LeafENtB1n_4EdgeE16insert_recursingNtNtBc_5alloc6GlobalNCNvMs6_NtNtB8_3map5entryINtB3b_11VacantEntryyB1E_E12insert_entry0ECsaNb1NCWtnRw_29awaken_session_state_contract.0', 1),
    # navigate.rs:634
    ('_RNvMsn_NtNtNtCs6NSMbdZCE7r_5alloc11collections5btree8navigateINtNtB7_4node7NodeRefNtNtB10_6marker5DyingyNtNtB7_7set_val9SetValZSTNtB1k_14LeafOrInternalE15first_leaf_edgeCsaNb1NCWtnRw_29awaken_session_state_contract.0', 1),
    # search.rs:57
    ('_RINvMs_NtNtNtCs6NSMbdZCE7r_5alloc11collections5btree6searchINtNtB7_4node7NodeRefNtNtBY_6marker5ImmutyNtNtB7_7set_val9SetValZSTNtB1i_14LeafOrInternalE11search_treeyECsaNb1NCWtnRw_29awaken_session_state_contract.0', 1),
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
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeNtNtNtCse2RmgB3TfrN_21awaken_agent_contract5agent7content12ContentBlockEBM_', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeSNtNtNtCse2RmgB3TfrN_21awaken_agent_contract5agent7content19SearchResultContentEBN_.0', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeSNtNtNtCse2RmgB3TfrN_21awaken_agent_contract5agent7content12ContentBlockEBN_.0', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeSNtNtCs5YUhLFlCshu_5alloc6string6StringECs6MUXvrZ2NgI_3std.0', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeSNtNtNtCse2RmgB3TfrN_21awaken_agent_contract5agent5state7CommandEBN_.0', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeSNtNtCsbpSFxJTBWlC_10serde_json5value5ValueEBL_.0', 1),
    ('_RINvNtCsjxLPRqQEPX2_4core3ptr13drop_in_placeSNtNtNtCse2RmgB3TfrN_21awaken_agent_contract5agent7message7MessageEBN_.0', 1),
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


@dataclass(frozen=True)
class Profile:
    """One audited option set. Consumer: ``formal_plan`` serializes it per batch.

    ``package`` binds a profile whose loop identities are mangled against one
    crate; ``None`` admits any package (a solver choice or a builtin symbol).
    """

    package: str | None
    loops: tuple[tuple[str, int], ...]
    solver: str | None


# Profiles bind solver options, and loop identities, to their production package.
# Harness membership is not listed here: each harness names its profile itself.
PROFILES: dict[str, Profile] = {
    "resume-validator-empty-object-v1": Profile(
        "awaken-runtime-contract", TOOL_RESULT_EMPTY_OBJECT_LOOPS, "cadical",
    ),
    "erasure-progress-selection-v1": Profile(
        "awaken-data-subject-application",
        ERASURE_PROGRESS_LOOPS + (
            # index_range.rs:167: first-match scan of two adapter references, plus exit.
            ('_RINvXs_NtNtCsjxLPRqQEPX2_4core3ops11index_rangeNtB5_10IndexRangeNtNtNtNtB9_4iter6traits8iterator8Iterator8try_folduNCINvMs6_NtNtNtB9_5array4iter10iter_innerINtB1Y_15PolymorphicIterSINtNtNtB9_3mem12maybe_uninit11MaybeUninitTNtCs7LhrEI5S3s3_31awaken_data_subject_application13ErasureTargetINtNtB9_6option6OptionRhEEEE8try_folduNCINvNvB10_4find5checkB3y_NCINvMs0_B3B_NtB3B_15ErasureProgress14pending_targetB4X_E0E0INtNtB7_12control_flow11ControlFlowB3y_EE0B6B_EB3B_.0', 3),
        ),
        "cadical",  # Explicitly retains Kani's default solver.
    ),
    "erasure-checkpoint-successor-v1": Profile(
        "awaken-data-subject-application",
        ERASURE_PROGRESS_LOOPS + (
            # slice/iter/macros.rs:314: all visits at most two old rows plus exit.
            ('_RINvXs2J_NtNtCsjxLPRqQEPX2_4core5slice4iterINtB7_4IterNtCs7LhrEI5S3s3_31awaken_data_subject_application13ErasureTargetENtNtNtNtBb_4iter6traits8iterator8Iterator3allNCNvMs0_BS_NtBS_15ErasureProgress7follows0EBS_.0', 3),
        ),
        "cadical",  # Same default solver; global 32 retains allocation and Drop.
    ),
    # Exact final wire encoding only: SHA/FNV algorithms and enclosing request
    # histories keep their separate obligations. Every default check stays on.
    "fingerprint-fnv-hex-v1": Profile(
        "awaken-agent-contract",
        # 24 ASCII wire bytes: at most seven alignment bytes, one 16-byte
        # block, eight tail bytes; bounds include exit. mod_inv needs <=4
        # iterations on the pinned 64-bit target. Global unwind remains 18.
        tuple(zip(FINGERPRINT_ENCODING_LOOPS, (2, 9, 9, 5))),
        "z3",
    ),
    "fingerprint-sha256-hex-v1": Profile(
        "awaken-agent-contract",
        # 71 ASCII wire bytes: <=7 alignment bytes and four 16-byte blocks;
        # tail 17 is conservative, outer 9 includes exit. Global unwind 66
        # retains every one of the 256 unconstrained input bits. CaDiCaL is
        # independently qualified; the Z3 timeout is not a proof result.
        tuple(zip(FINGERPRINT_ENCODING_LOOPS, (5, 17, 9, 5))),
        "cadical",
    ),
    "activity-admission-height-v1": Profile(
        "awaken-session-state-contract", ACTIVITY_HEIGHT_LOOPS, None,
    ),
    "activity-terminal-height-v1": Profile(
        "awaken-session-state-contract", ACTIVITY_HEIGHT_LOOPS[:7], None,
    ),
    # Real two-ID output reversal swaps three 8-byte chunks of each String.
    # The bound and solver are owned by this profile, as the ledger states.
    "retained-accepted-two-replay-v1": Profile(
        "awaken-session-application-contract",
        (
            ("memcmp.0", 2),
            # core/ptr/mod.rs:1452: 24-byte String swaps three 8-byte chunks;
            # bound 4 includes the loop exit. Global unwind 3 stays unchanged.
            ("_RINvNvNtCsjxLPRqQEPX2_4core3ptr25swap_nonoverlapping_bytes26swap_nonoverlapping_chunksKj8_ECs6MUXvrZ2NgI_3std.0", 4),
        ),
        "z3",
    ),
    # Builtin `memcmp` loop budgets. Every unwinding assertion stays on; only
    # the string comparison's loop gets the bound (bound includes loop exit).
    "unwind-memcmp-2-v1": Profile(None, (("memcmp.0", 2),), None),
    "unwind-memcmp-64-v1": Profile(None, (("memcmp.0", 64),), None),
    # A complete 64-byte conflict message also needs its loop-exit check.
    "unwind-memcmp-65-v1": Profile(None, (("memcmp.0", 65),), None),
    # Two-slot material collections do not need the string comparison's loop
    # budget: only builtin memcmp gets 16. The owned predecessor-set iterator
    # uses the bundled MiniSAT backend.
    "minisat-unwind-memcmp-16-v1": Profile(None, (("memcmp.0", 16),), "minisat"),
    # Solver choices alone. CaDiCaL and MiniSAT are compiled into CBMC; Z3 must
    # be the pinned version; kissat ships in the released Kani bundle.
    "solver-cadical-v1": Profile(None, (), "cadical"),
    "solver-kissat-v1": Profile(None, (), "kissat"),
    "solver-minisat-v1": Profile(None, (), "minisat"),
    "solver-z3-v1": Profile(None, (), "z3"),
}


def serialize(name: str, package: str, selection: list[str]) -> list[str]:
    """The Kani argv of profile ``name`` around ``selection`` (the harness filter).

    ``--cbmc-args`` consumes the rest of the command line, so the selection
    sits between the solver and it. A loop profile needs ``-Z unstable-options``;
    a solver-only profile does not, and its argv carries no loop override.
    """

    profile = PROFILES.get(name)
    if profile is None:
        raise ValueError(f"unknown Kani profile {name!r}")
    if profile.package is not None and profile.package != package:
        raise ValueError(f"Kani profile {name} is bound to package {profile.package}, not {package}")
    solver = ["--solver", profile.solver] if profile.solver is not None else []
    if not profile.loops:
        return [*solver, *selection]
    return ["-Z", "unstable-options", *solver, *selection,
            "--cbmc-args", "--unwindset", ",".join(f"{loop}:{bound}" for loop, bound in profile.loops)]
