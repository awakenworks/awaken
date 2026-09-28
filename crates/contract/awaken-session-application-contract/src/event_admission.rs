// @formal entrypoint session_event_batch.final_non_admission_excludes_late_acceptance
//! Minimal retained admission evidence. No message, tool input or execution log.
#[cfg(any(test, kani))]
use crate::RetainedEventAdmission;
use crate::{DeletedSessionEventAdmissions, SessionEventBatch, SessionRevision};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEventAdmission {
    Accepted {
        batch_id: String,
        /// None is a legacy retained receipt with no observation coordinate.
        operation_ids: Option<Vec<String>>,
    },
    Closed {
        batch_id: String,
    },
    Deleted {
        deleted_revision: SessionRevision,
    },
}

impl From<&SessionEventBatch> for SessionEventAdmission {
    fn from(batch: &SessionEventBatch) -> Self {
        if batch.admission_closed {
            Self::Closed {
                batch_id: batch.batch_id.clone(),
            }
        } else {
            Self::Accepted {
                batch_id: batch.batch_id.clone(),
                operation_ids: Some(batch.operation_ids()),
            }
        }
    }
}

pub fn resolve_deleted_event_admission(
    proof: &DeletedSessionEventAdmissions,
    key: &str,
    fingerprint: &str,
) -> Result<SessionEventAdmission, crate::RunError> {
    let admissions = proof.admissions.as_ref().ok_or_else(|| {
        crate::RunError::unavailable("Legacy Session deletion has no retained admission proof")
    })?;
    let receipt = admissions.iter().find(|receipt| receipt.key == key);
    // Retention owns lookup/payloads; the same canonical admission class
    // as the live root owns fingerprint-before-closed precedence.
    match crate::classify_session_event_batch_admission(
        receipt.is_some(),
        receipt.is_some_and(|receipt| receipt.request_fingerprint == fingerprint),
        receipt.is_some_and(|receipt| receipt.closed),
    ) {
        crate::SessionEventBatchAdmissionClass::Conflict => Err(event_admission_conflict()),
        crate::SessionEventBatchAdmissionClass::Closed => Ok(SessionEventAdmission::Closed {
            batch_id: receipt
                .expect("closed class has retained evidence")
                .batch_id
                .clone(),
        }),
        crate::SessionEventBatchAdmissionClass::Accepted => {
            let receipt = receipt.expect("accepted class has retained evidence");
            Ok(SessionEventAdmission::Accepted {
                batch_id: receipt.batch_id.clone(),
                operation_ids: receipt.operation_ids.clone(),
            })
        }
        crate::SessionEventBatchAdmissionClass::Absent => Ok(SessionEventAdmission::Deleted {
            deleted_revision: proof.deleted_revision,
        }),
    }
}

pub fn event_admission_conflict() -> crate::RunError {
    crate::RunError {
        message: "Idempotency-Key was already used for another Session Event batch".into(),
        kind: crate::RunErrorKind::BadRequest,
        code: "idempotency_conflict".into(),
    }
}

pub fn event_admission_closed() -> crate::RunError {
    crate::RunError {
        message: "Session Event batch admission was permanently closed".into(),
        kind: crate::RunErrorKind::BadRequest,
        code: "event_admission_closed".into(),
    }
}

#[cfg(any(test, kani))]
mod verification {
    use super::*;

    // This oracle states each observable outcome's necessary and sufficient
    // causes. The caller supplies the expected first index independently; it
    // never searches with the production iterator or calls its error factory.
    fn check_result(
        proof: &DeletedSessionEventAdmissions,
        first: Option<usize>,
        fingerprint: &str,
        result: &Result<SessionEventAdmission, crate::RunError>,
    ) {
        let selected = first.map(|index| &proof.admissions.as_ref().unwrap()[index]);
        match result {
            Err(error) => {
                if proof.admissions.is_none() {
                    assert!(
                        error.kind == crate::RunErrorKind::Unavailable,
                        "exact retained admission field"
                    );
                    assert!(
                        error.code == "unavailable",
                        "exact retained admission field"
                    );
                    assert!(
                        error.message == "Legacy Session deletion has no retained admission proof",
                        "exact retained admission field"
                    );
                } else {
                    let receipt =
                        selected.expect("complete inventory errors require a matching key");
                    assert!(receipt.request_fingerprint != fingerprint);
                    assert!(
                        error.kind == crate::RunErrorKind::BadRequest,
                        "exact retained admission field"
                    );
                    assert!(
                        error.code == "idempotency_conflict",
                        "exact retained admission field"
                    );
                    assert!(
                        error.message
                            == "Idempotency-Key was already used for another Session Event batch",
                        "exact retained admission field"
                    );
                }
            }
            Ok(SessionEventAdmission::Deleted { deleted_revision }) => {
                assert!(proof.admissions.is_some());
                assert!(first.is_none());
                assert!(
                    *deleted_revision == proof.deleted_revision,
                    "exact retained admission field"
                );
            }
            Ok(SessionEventAdmission::Closed { batch_id }) => {
                let receipt = selected.expect("closed requires retained evidence");
                assert!(
                    receipt.request_fingerprint == fingerprint,
                    "exact retained admission field"
                );
                assert!(receipt.closed);
                assert!(
                    *batch_id == receipt.batch_id,
                    "exact retained admission field"
                );
            }
            Ok(SessionEventAdmission::Accepted {
                batch_id,
                operation_ids,
            }) => {
                let receipt = selected.expect("acceptance requires retained evidence");
                assert!(
                    receipt.request_fingerprint == fingerprint,
                    "exact retained admission field"
                );
                assert!(!receipt.closed);
                assert!(
                    *batch_id == receipt.batch_id,
                    "exact retained admission field"
                );
                assert!(
                    *operation_ids == receipt.operation_ids,
                    "exact retained admission field"
                );
            }
        }
    }

    fn check_replay_and_frame(
        proof: DeletedSessionEventAdmissions,
        key: &str,
        fingerprint: &str,
        first: Option<usize>,
    ) {
        let before = proof.clone();
        check_replay_with_frame(proof, key, fingerprint, first, |current| {
            assert!(
                *current == before,
                "independent complete input frame agrees"
            );
        });
    }

    fn check_replay_with_frame(
        proof: DeletedSessionEventAdmissions,
        key: &str,
        fingerprint: &str,
        first: Option<usize>,
        check_frame: impl Fn(&DeletedSessionEventAdmissions),
    ) {
        let mut result = check_first_result(&proof, key, fingerprint, first, &check_frame);
        // Mutating the returned owned clone must not poison retained evidence
        // or the next replay, including ordered operation ids and batch id.
        match &mut result {
            Ok(SessionEventAdmission::Accepted {
                batch_id,
                operation_ids,
            }) => {
                batch_id.clear();
                if let Some(ids) = operation_ids {
                    ids.reverse();
                    if let Some(id) = ids.first_mut() {
                        id.clear();
                    }
                }
            }
            Ok(SessionEventAdmission::Closed { batch_id }) => batch_id.clear(),
            _ => {}
        }
        check_frame(&proof);
        check_result(
            &proof,
            first,
            fingerprint,
            &resolve_deleted_event_admission(&proof, key, fingerprint),
        );
        check_frame(&proof);
    }

    fn check_first_result(
        proof: &DeletedSessionEventAdmissions,
        key: &str,
        fingerprint: &str,
        first: Option<usize>,
        check_frame: &impl Fn(&DeletedSessionEventAdmissions),
    ) -> Result<SessionEventAdmission, crate::RunError> {
        check_frame(proof);
        let result = resolve_deleted_event_admission(proof, key, fingerprint);
        check_frame(proof);
        check_result(proof, first, fingerprint, &result);
        result
    }

    // Legacy-inventory cause R1: one actual resolve must return the exact
    // Unavailable error, preserve the complete source and destroy the owned
    // error normally. The original replay oracle separately covers two live
    // outputs in native tests; this helper does not replace that lifetime case.
    fn check_legacy_error_and_frame(
        proof: DeletedSessionEventAdmissions,
        key: &str,
        fingerprint: &str,
    ) {
        assert!(proof.admissions.is_none());
        let before = proof.clone();
        let result = resolve_deleted_event_admission(&proof, key, fingerprint);
        check_result(&before, None, fingerprint, &result);
        assert!(proof == before);
        drop(result);
        assert!(proof == before);
    }

    #[cfg(test)]
    #[test]
    fn deleted_admission_decision_table() {
        // Causes C1 legacy None / complete inventory lengths 0,1,2; C2 first
        // matching key at none/0/1 (including duplicate keys); C3 fingerprint
        // same/different independently per receipt; C4 each receipt closed;
        // C5 operation ids None/Some(empty)/Some(one)/Some(two ordered ids).
        // R1 !C1 => exact Unavailable. R2 complete/no key => full revision.
        // R3 first key + different fingerprint => exact conflict even if closed.
        // R4/R5 first key + same fingerprint => original closed/accepted receipt.
        // Every rule checks source equality, mutation-isolated owned output and
        // a second real resolve. The first-index table is independent of find.
        let mut cases = 0;
        for revision in [0, 1, u64::MAX] {
            // R1 cross-checks the exact single-call/Drop proof oracle as well
            // as the unchanged two-live-output replay table below.
            check_legacy_error_and_frame(
                DeletedSessionEventAdmissions {
                    deleted_revision: SessionRevision(revision),
                    admissions: None,
                },
                "k",
                "fp",
            );
            for admissions in [None, Some(vec![])] {
                check_replay_and_frame(
                    DeletedSessionEventAdmissions {
                        deleted_revision: SessionRevision(revision),
                        admissions,
                    },
                    "k",
                    "fp",
                    None,
                );
                cases += 1;
            }
            // Only occupied slots contribute causes: no phantom second-slot
            // flags in the one-receipt rows. Each receipt owns independent ids.
            for len in [1, 2] {
                for keys in 0..(1 << len) {
                    let first = if keys & 1 != 0 {
                        Some(0)
                    } else if len == 2 && keys & 2 != 0 {
                        Some(1)
                    } else {
                        None
                    };
                    for flags in 0..(1 << (2 * len)) {
                        for ids in 0..(1 << (2 * len)) {
                            let receipts = (0..len)
                                .map(|index| RetainedEventAdmission {
                                    key: if keys & (1 << index) == 0 {
                                        "other"
                                    } else {
                                        "k"
                                    }
                                    .into(),
                                    request_fingerprint: if flags & (1 << index) == 0 {
                                        "fp"
                                    } else {
                                        "changed"
                                    }
                                    .into(),
                                    batch_id: format!("batch:{index}"),
                                    closed: flags & (1 << (len + index)) != 0,
                                    operation_ids: match (ids >> (2 * index)) & 3 {
                                        0 => None,
                                        1 => Some(vec![]),
                                        2 => Some(vec![format!("{index}:op:0")]),
                                        _ => Some(vec![
                                            format!("{index}:op:0"),
                                            format!("{index}:op:1"),
                                        ]),
                                    },
                                })
                                .collect();
                            check_replay_and_frame(
                                DeletedSessionEventAdmissions {
                                    deleted_revision: SessionRevision(revision),
                                    admissions: Some(receipts),
                                },
                                "k",
                                "fp",
                                first,
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 3 * (2 + 32 + 1024));
    }

    #[cfg(kani)]
    fn short_str() -> &'static str {
        match kani::any::<u8>() % 5 {
            0 => "",
            1 => "a",
            2 => "b",
            3 => "ab",
            _ => "ba",
        }
    }

    // The actual deleted resolver covers empty-inventory replay and one legacy
    // error/frame/Drop call. Legacy replay remains open.
    // Bounded actual resolver proof: Some(empty), all u64 revisions and
    // request key/fingerprint in {empty,a,b,ab,ba}. It must return Deleted with
    // the exact revision, preserve the complete source and repeat identically.
    // Real Vec/String/Result operations and Drop retain all safety checks.
    // This partition excludes legacy None and one/two retained receipts; it
    // does not prove the whole resolver or SQL codec/transaction behavior.
    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-64-v1
    #[kani::proof]
    #[kani::unwind(4)]
    fn deleted_admission_resolver_preserves_empty() {
        check_replay_and_frame(
            DeletedSessionEventAdmissions {
                deleted_revision: SessionRevision(kani::any()),
                admissions: Some(vec![]),
            },
            short_str(),
            short_str(),
            None,
        );
    }
    // One real legacy None resolve, all u64 revisions and independent query
    // classes {empty,a,b,ab,ba}: exact error kind/code/message, complete source
    // frame and real Drop. Sequential replay and two coexisting error outputs
    // remain separate unresolved allocator-path proofs, as do nonempty inputs.
    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-64-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(4)]
    fn deleted_admission_resolver_preserves_legacy_error_and_frame() {
        check_legacy_error_and_frame(
            DeletedSessionEventAdmissions {
                deleted_revision: SessionRevision(kani::any()),
                admissions: None,
            },
            short_str(),
            short_str(),
        );
    }
    // C1 key equality, C2 fingerprint equality and C3 closed are independent
    // bits; C4 distinguishes None/empty/one/two ordered operation ids. The 32
    // literal partitions cover absent=16, conflict=8, closed=4, accepted=4.
    // Each retains every u64 revision and the same complete replay oracle.
    // Literal fixtures avoid a second owned input tree; real output Clone,
    // mutation, two-live-output replay and Drop stay intact.
    fn check_literal_frame<const CASE: u8>(proof: &DeletedSessionEventAdmissions, revision: u64) {
        assert_eq!(proof.deleted_revision, SessionRevision(revision));
        let rows = proof.admissions.as_ref().expect("literal inventory");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.key, if CASE & 1 == 0 { "b" } else { "a" });
        assert_eq!(
            row.request_fingerprint,
            if CASE & 2 == 0 { "b" } else { "a" }
        );
        assert_eq!(row.batch_id, "0");
        assert_eq!(row.closed, CASE & 4 != 0);
        match CASE >> 3 {
            0 => assert!(row.operation_ids.is_none()),
            1 => assert!(row.operation_ids.as_ref().is_some_and(Vec::is_empty)),
            2 => {
                let ids = row.operation_ids.as_ref().expect("one operation");
                assert_eq!(ids.len(), 1);
                assert_eq!(ids[0], "x");
            }
            _ => {
                let ids = row.operation_ids.as_ref().expect("two operations");
                assert_eq!(ids.len(), 2);
                assert_eq!(ids[0], "x");
                assert_eq!(ids[1], "y");
            }
        }
    }

    fn literal_fixture<const CASE: u8>(revision: u64) -> DeletedSessionEventAdmissions {
        assert!(CASE < 32);
        let receipt = || RetainedEventAdmission {
            key: if CASE & 1 == 0 { "b" } else { "a" }.into(),
            request_fingerprint: if CASE & 2 == 0 { "b" } else { "a" }.into(),
            batch_id: "0".into(),
            closed: CASE & 4 != 0,
            operation_ids: match CASE >> 3 {
                0 => None,
                1 => Some(vec![]),
                2 => Some(vec!["x".into()]),
                _ => Some(vec!["x".into(), "y".into()]),
            },
        };
        DeletedSessionEventAdmissions {
            deleted_revision: SessionRevision(revision),
            admissions: Some(vec![receipt()]),
        }
    }

    fn check_single_retained_literal<const CASE: u8>(revision: u64) {
        let proof = literal_fixture::<CASE>(revision);
        check_replay_with_frame(
            proof,
            "a",
            "a",
            if CASE & 1 == 0 { None } else { Some(0) },
            |current| {
                check_literal_frame::<CASE>(current, revision);
            },
        );
    }

    #[cfg(test)]
    #[test]
    fn deleted_admission_literal_partition_crosscheck() {
        // Causes: C1 key matches; C2 fingerprint matches; C3 receipt is closed;
        // C4 operation ids are None/empty/one/two; C5 revision is 0/1/MAX.
        // Effects: absent -> exact Deleted revision; mismatch -> Conflict;
        // closed -> Closed; open match -> Accepted with ordered owned ids.
        // Every rule also requires the complete source frame before/after
        // mutation and an exact second result while the first remains alive.
        // R1=!C1; R2=C1+!C2; R3=C1+C2+C3; R4=C1+C2+!C3;
        // C4 and C5 cross every rule. The broader 3,174-row table covers
        // additional strings, legacy/empty inventories and two receipts.
        for revision in [0, 1, u64::MAX] {
            check_single_retained_literal::<0>(revision);
            check_single_retained_literal::<1>(revision);
            check_single_retained_literal::<2>(revision);
            check_single_retained_literal::<3>(revision);
            check_single_retained_literal::<4>(revision);
            check_single_retained_literal::<5>(revision);
            check_single_retained_literal::<6>(revision);
            check_single_retained_literal::<7>(revision);
            check_single_retained_literal::<8>(revision);
            check_single_retained_literal::<9>(revision);
            check_single_retained_literal::<10>(revision);
            check_single_retained_literal::<11>(revision);
            check_single_retained_literal::<12>(revision);
            check_single_retained_literal::<13>(revision);
            check_single_retained_literal::<14>(revision);
            check_single_retained_literal::<15>(revision);
            check_single_retained_literal::<16>(revision);
            check_single_retained_literal::<17>(revision);
            check_single_retained_literal::<18>(revision);
            check_single_retained_literal::<19>(revision);
            check_single_retained_literal::<20>(revision);
            check_single_retained_literal::<21>(revision);
            check_single_retained_literal::<22>(revision);
            check_single_retained_literal::<23>(revision);
            check_single_retained_literal::<24>(revision);
            check_single_retained_literal::<25>(revision);
            check_single_retained_literal::<26>(revision);
            check_single_retained_literal::<27>(revision);
            check_single_retained_literal::<28>(revision);
            check_single_retained_literal::<29>(revision);
            check_single_retained_literal::<30>(revision);
            check_single_retained_literal::<31>(revision);
        }
    }

    // Qualified occupied cases cover all 24 None/empty/one-ID combinations
    // plus every two-ID Absent/Conflict/Closed case. All retain fixed one-byte
    // strings, every u64 revision and the complete source/replay frame.
    // Accepted one-ID output is cleared before replay; the added two-ID cases
    // do not return operation IDs, so two-ID output ordering remains native-only.
    // Accepted27 and all two-receipt inventories remain unproved by Kani.
    // Cases 25/26/29 pass with a measured 600s budget, unchanged inputs/checks;
    // Accepted27 needs swap-chunk bound 4, whose full probe hit the RSS guard.
    // Bound 3 retains the two-ID slice comparison's loop exit (bound 2 failed);
    // the other literals keep bound 2. All default checks remain enabled.
    // Conflict's complete 64-byte message needs memcmp bound 65 for loop exit;
    // the bound-64 qualification failed its unwinding assertion, not silently
    // truncating the error comparison. Nonerror calls use memcmp bound 2.
    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_closed_none_07() {
        check_single_retained_literal::<7>(kani::any());
    }
    // All 24 None/empty/one-ID cases plus all two-ID Absent/Conflict/Closed;
    // full source frame and two-live-output replay/Drop (two-ID output stays open).
    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_absent_none_00() {
        check_single_retained_literal::<0>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-65-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_conflict_none_01() {
        check_single_retained_literal::<1>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_absent_none_02() {
        check_single_retained_literal::<2>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_accepted_none_03() {
        check_single_retained_literal::<3>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_absent_none_04() {
        check_single_retained_literal::<4>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-65-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_conflict_none_05() {
        check_single_retained_literal::<5>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_absent_none_06() {
        check_single_retained_literal::<6>(kani::any());
    }
    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_absent_empty_08() {
        check_single_retained_literal::<8>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-65-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_conflict_empty_09() {
        check_single_retained_literal::<9>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_absent_empty_10() {
        check_single_retained_literal::<10>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_accepted_empty_11() {
        check_single_retained_literal::<11>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_absent_empty_12() {
        check_single_retained_literal::<12>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-65-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_conflict_empty_13() {
        check_single_retained_literal::<13>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_absent_empty_14() {
        check_single_retained_literal::<14>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_closed_empty_15() {
        check_single_retained_literal::<15>(kani::any());
    }
    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_absent_one_16() {
        check_single_retained_literal::<16>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-65-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_conflict_one_17() {
        check_single_retained_literal::<17>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_absent_one_18() {
        check_single_retained_literal::<18>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_accepted_one_19() {
        check_single_retained_literal::<19>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_absent_one_20() {
        check_single_retained_literal::<20>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-65-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_conflict_one_21() {
        check_single_retained_literal::<21>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_absent_one_22() {
        check_single_retained_literal::<22>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1
    #[kani::proof]
    #[kani::unwind(2)]
    fn deleted_admission_one_retained_closed_one_23() {
        check_single_retained_literal::<23>(kani::any());
    }
    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(3)]
    fn deleted_admission_one_retained_absent_two_24() {
        check_single_retained_literal::<24>(kani::any());
    }

    // CASE27: matching key/fingerprint, open retained receipt, two ordered
    // IDs and every u64 revision. Reuse the complete two-live-output oracle:
    // actual owned Clone/reverse/clear/replay/Drop and all source fields.
    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=retained-accepted-two-replay-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(3)]
    fn deleted_admission_one_retained_accepted_two_27() {
        check_single_retained_literal::<27>(kani::any());
    }

    // The first-result lemma completes on the real resolver before the larger
    // owned-mutation/replay harness; it does not substitute for that proof.
    // CASE27 first-result lemma. C1 the retained key/fingerprint match and
    // the receipt is open; C2 it carries two ordered IDs; C3 revision is any
    // u64. E1 the real resolver returns exact owned Accepted fields; E2 the
    // complete retained source is unchanged. This is the first stage of the
    // existing replay oracle, not a proof-only replacement implementation.
    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    #[kani::proof]
    #[kani::unwind(3)]
    fn deleted_admission_one_retained_accepted_two_initial_27() {
        let revision = kani::any();
        let proof = literal_fixture::<27>(revision);
        let check_frame = |current: &DeletedSessionEventAdmissions| {
            check_literal_frame::<27>(current, revision);
        };
        let _result = check_first_result(&proof, "a", "a", Some(0), &check_frame);
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(3)]
    fn deleted_admission_one_retained_absent_two_28() {
        check_single_retained_literal::<28>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(3)]
    fn deleted_admission_one_retained_absent_two_30() {
        check_single_retained_literal::<30>(kani::any());
    }

    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(3)]
    fn deleted_admission_one_retained_closed_two_31() {
        check_single_retained_literal::<31>(kani::any());
    }
    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-2-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(3)]
    fn deleted_admission_one_retained_absent_two_26() {
        check_single_retained_literal::<26>(kani::any());
    }
    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-65-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(3)]
    fn deleted_admission_one_retained_conflict_two_25() {
        check_single_retained_literal::<25>(kani::any());
    }
    #[cfg(kani)]
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @kani profile=unwind-memcmp-65-v1 tier=nightly
    #[kani::proof]
    #[kani::unwind(3)]
    fn deleted_admission_one_retained_conflict_two_29() {
        check_single_retained_literal::<29>(kani::any());
    }
}
