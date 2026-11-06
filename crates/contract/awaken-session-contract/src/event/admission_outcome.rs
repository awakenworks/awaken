// @formal entrypoint session_event_batch.final_non_admission_excludes_late_acceptance
//! The outcome of one Session Event batch admission, including the replay of
//! minimal retained admission evidence after deletion. No message, tool input
//! or execution log.
#[cfg(any(test, kani))]
use crate::RetainedEventAdmission;
use crate::{DeletedSessionEventAdmissions, SessionEventBatch};
use awaken_runtime_contract::runs::{RunError, RunErrorKind};
use awaken_session_types::SessionRevision;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEventAdmission {
    /// Accepted without a batch or executable operation.
    NoOp,
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
) -> Result<SessionEventAdmission, RunError> {
    let admissions = proof.admissions.as_ref().ok_or_else(|| {
        RunError::unavailable("Legacy Session deletion has no retained admission proof")
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
                .clone()
                .ok_or_else(|| {
                    RunError::unavailable("Closed Event receipt has no retained batch identity")
                })?,
        }),
        crate::SessionEventBatchAdmissionClass::Accepted => {
            let receipt = receipt.expect("accepted class has retained evidence");
            if receipt.batch_id.is_none() {
                return if receipt.operation_ids.as_ref().is_some_and(Vec::is_empty) {
                    Ok(SessionEventAdmission::NoOp)
                } else {
                    Err(RunError::unavailable(
                        "Event no-op receipt has no known-empty operation proof",
                    ))
                };
            }
            Ok(SessionEventAdmission::Accepted {
                batch_id: receipt
                    .batch_id
                    .clone()
                    .expect("checked retained batch identity"),
                operation_ids: receipt.operation_ids.clone(),
            })
        }
        crate::SessionEventBatchAdmissionClass::Absent => Ok(SessionEventAdmission::Deleted {
            deleted_revision: proof.deleted_revision,
        }),
    }
}

pub fn event_admission_conflict() -> RunError {
    RunError {
        message: "Idempotency-Key was already used for another Session Event batch".into(),
        kind: RunErrorKind::BadRequest,
        code: "idempotency_conflict".into(),
    }
}

pub fn event_admission_closed() -> RunError {
    RunError {
        message: "Session Event batch admission was permanently closed".into(),
        kind: RunErrorKind::BadRequest,
        code: "event_admission_closed".into(),
    }
}

#[cfg(any(test, kani))]
mod verification {
    use super::*;

    #[test]
    fn deleted_noop_requires_explicit_known_empty_proof_and_keeps_legacy_unknown() {
        // DT-D: C1 no batch identity; C2 known-empty/unknown operation proof;
        // C3 matching/different fingerprint. D1 C1+empty+match => NoOp;
        // D2 C1+unknown => unavailable; D3 changed => conflict; D4 a legacy
        // retained batch with omitted ids remains Accepted with unknown ids.
        let row = crate::RetainedEventAdmission {
            key: "k".into(),
            request_fingerprint: "f".into(),
            batch_id: None,
            closed: false,
            operation_ids: Some(Vec::new()),
        };
        let proof = |row| DeletedSessionEventAdmissions {
            deleted_revision: SessionRevision(7),
            admissions: Some(vec![row]),
        };
        assert_eq!(
            resolve_deleted_event_admission(&proof(row.clone()), "k", "f").unwrap(),
            SessionEventAdmission::NoOp
        );
        assert_eq!(
            resolve_deleted_event_admission(&proof(row.clone()), "k", "changed")
                .unwrap_err()
                .code,
            "idempotency_conflict"
        );
        let mut unknown = row.clone();
        unknown.operation_ids = None;
        assert_eq!(
            resolve_deleted_event_admission(&proof(unknown), "k", "f")
                .unwrap_err()
                .kind,
            RunErrorKind::Unavailable
        );
        let mut legacy = row;
        legacy.batch_id = Some("batch".into());
        legacy.operation_ids = None;
        assert_eq!(
            resolve_deleted_event_admission(&proof(legacy), "k", "f").unwrap(),
            SessionEventAdmission::Accepted {
                batch_id: "batch".into(),
                operation_ids: None
            }
        );
        // DT-N: C1 matching/different fingerprint; C2 open/closed; C3
        // absent/known-empty/nonempty ids, all with no batch. N1 !C1 =>
        // Conflict before malformed proof; N2 C1+open+empty => NoOp;
        // N3 C1+closed or C1+open+nonempty/unknown => exact Unavailable.
        // All 12 rows reuse main's complete result/frame/replay oracle.
        for fingerprint in ["f", "changed"] {
            for closed in [false, true] {
                for operation_ids in [None, Some(vec![]), Some(vec!["op".into()])] {
                    check_replay_and_frame(
                        proof(crate::RetainedEventAdmission {
                            key: "k".into(),
                            request_fingerprint: "f".into(),
                            batch_id: None,
                            closed,
                            operation_ids,
                        }),
                        "k",
                        fingerprint,
                        Some(0),
                    );
                }
            }
        }
    }

    // C/E table: R1 legacy inventory => exact Unavailable; R2 complete/no
    // first key => exact Deleted revision; R3 first fingerprint differs =>
    // exact Conflict even if closed; R4/R5 matching first receipt => exact
    // Closed/Accepted payload, including None versus ordered operation ids.
    // R6 matching open receipt with no batch and known-empty ids => NoOp;
    // R7 closed/malformed batchless evidence => exact Unavailable.
    // The caller supplies the first index independently of production lookup.
    // A missing/out-of-range claimed first is invalid, never silently absent.
    // One total postcondition is checked in every literal partition: branches
    // select Boolean conditions, never assertions for another partition.
    fn check_result(
        proof: &DeletedSessionEventAdmissions,
        first: Option<usize>,
        fingerprint: &str,
        result: &Result<SessionEventAdmission, RunError>,
    ) {
        let selected = proof
            .admissions
            .as_ref()
            .and_then(|rows| first.and_then(|index| rows.get(index)));
        let valid_first = first.is_none() || selected.is_some();
        let outcome_matches = match result {
            Err(error) => {
                let legacy = proof.admissions.is_none()
                    && error.kind == RunErrorKind::Unavailable
                    && error.code == "unavailable"
                    && error.message == "Legacy Session deletion has no retained admission proof";
                let conflict = proof.admissions.is_some()
                    && selected.is_some_and(|receipt| receipt.request_fingerprint != fingerprint)
                    && error.kind == RunErrorKind::BadRequest
                    && error.code == "idempotency_conflict"
                    && error.message
                        == "Idempotency-Key was already used for another Session Event batch";
                let invalid_receipt = selected.is_some_and(|receipt| {
                    receipt.request_fingerprint == fingerprint
                        && receipt.batch_id.is_none()
                        && if receipt.closed {
                            error.message == "Closed Event receipt has no retained batch identity"
                        } else {
                            !receipt.operation_ids.as_ref().is_some_and(Vec::is_empty)
                                && error.message
                                    == "Event no-op receipt has no known-empty operation proof"
                        }
                }) && error.kind == RunErrorKind::Unavailable
                    && error.code == "unavailable";
                legacy || conflict || invalid_receipt
            }
            Ok(SessionEventAdmission::Deleted { deleted_revision }) => {
                proof.admissions.is_some()
                    && first.is_none()
                    && *deleted_revision == proof.deleted_revision
            }
            Ok(SessionEventAdmission::Closed { batch_id }) => selected.is_some_and(|receipt| {
                receipt.request_fingerprint == fingerprint
                    && receipt.closed
                    && Some(batch_id.as_str()) == receipt.batch_id.as_deref()
            }),
            Ok(SessionEventAdmission::Accepted {
                batch_id,
                operation_ids,
            }) => selected.is_some_and(|receipt| {
                receipt.request_fingerprint == fingerprint
                    && !receipt.closed
                    && Some(batch_id.as_str()) == receipt.batch_id.as_deref()
                    && *operation_ids == receipt.operation_ids
            }),
            Ok(SessionEventAdmission::NoOp) => selected.is_some_and(|receipt| {
                receipt.request_fingerprint == fingerprint
                    && !receipt.closed
                    && receipt.batch_id.is_none()
                    && receipt.operation_ids.as_ref().is_some_and(Vec::is_empty)
            }),
        };
        assert!(
            valid_first && outcome_matches,
            "exact retained admission outcome and first index"
        );
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
    ) -> Result<SessionEventAdmission, RunError> {
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
                                    batch_id: Some(format!("batch:{index}")),
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
        // C6 inventory is exactly one borrowed row; C7 ids are independently
        // None/[]/[x]/[x,y] as CASE selects. Every complete frame coordinate
        // contributes to this one mandatory postcondition, even when another
        // literal partition cannot take its data branch. Slice patterns keep
        // wrong lengths/missing data false without proof-only panic paths.
        let expected_key = if CASE & 1 == 0 { "b" } else { "a" };
        let expected_fingerprint = if CASE & 2 == 0 { "b" } else { "a" };
        let retained_matches = match proof.admissions.as_deref() {
            Some([row]) => {
                let ids_match = match CASE >> 3 {
                    0 => row.operation_ids.is_none(),
                    1 => row
                        .operation_ids
                        .as_deref()
                        .is_some_and(|ids| matches!(ids, [])),
                    2 => row
                        .operation_ids
                        .as_deref()
                        .is_some_and(|ids| matches!(ids, [id] if id == "x")),
                    _ => row.operation_ids.as_deref().is_some_and(
                        |ids| matches!(ids, [first, second] if first == "x" && second == "y"),
                    ),
                };
                row.key == expected_key
                    && row.request_fingerprint == expected_fingerprint
                    && row.batch_id.as_deref() == Some("0")
                    && row.closed == (CASE & 4 != 0)
                    && ids_match
            }
            _ => false,
        };
        assert!(
            proof.deleted_revision == SessionRevision(revision) && retained_matches,
            "complete literal retained admission frame"
        );
    }

    fn literal_fixture<const CASE: u8>(revision: u64) -> DeletedSessionEventAdmissions {
        assert!(CASE < 32);
        let receipt = || RetainedEventAdmission {
            key: if CASE & 1 == 0 { "b" } else { "a" }.into(),
            request_fingerprint: if CASE & 2 == 0 { "b" } else { "a" }.into(),
            batch_id: Some("0".into()),
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

    #[cfg(test)]
    #[test]
    fn deleted_admission_oracle_rejects_each_corrupt_error_field_and_selection() {
        // Negative C/E: start with actual valid legacy/conflict results. Copy
        // one error field at a time; kind/code/message must each reject. A
        // false first index or inventory/fingerprint change also rejects.
        // The same check_result is the only oracle; catch is Native-only.
        let rejected = |proof: &DeletedSessionEventAdmissions,
                        first: Option<usize>,
                        result: &Result<SessionEventAdmission, RunError>| {
            assert!(std::panic::catch_unwind(|| check_result(proof, first, "a", result)).is_err());
        };
        for proof in [
            DeletedSessionEventAdmissions {
                deleted_revision: SessionRevision(17),
                admissions: None,
            },
            literal_fixture::<5>(17),
        ] {
            let first = proof.admissions.as_ref().map(|_| 0);
            let result = resolve_deleted_event_admission(&proof, "a", "a");
            check_result(&proof, first, "a", &result);
            let error = result.as_ref().unwrap_err();
            for field in 0..3 {
                // RunError has no Clone; copy all three actual fields before
                // changing exactly one, rather than manufacture an expected error.
                let mut changed = RunError {
                    kind: error.kind,
                    code: error.code.clone(),
                    message: error.message.clone(),
                };
                match field {
                    0 => changed.kind = RunErrorKind::Internal,
                    1 => changed.code.push('!'),
                    _ => changed.message.push('!'),
                }
                rejected(&proof, first, &Err(changed));
            }
            let mut changed = proof.clone();
            if proof.admissions.is_none() {
                rejected(&proof, Some(0), &result);
                changed.admissions = Some(vec![]);
                rejected(&changed, None, &result);
            } else {
                changed.admissions.as_mut().unwrap()[0].request_fingerprint = "a".into();
                rejected(&changed, first, &result);
            }
        }
    }

    #[cfg(test)]
    #[test]
    fn deleted_admission_oracle_rejects_corrupt_outcomes_and_receipt_authority() {
        // Single-fault rows: Deleted revision/inventory, Closed batch/fp/closed,
        // Accepted batch/fp/closed and absent/invalid first receipt. A forged
        // NoOp must fail for a retained batch or absent receipt. A real NoOp
        // rejects fabricated batch/deletion outcomes and each independent
        // batch/closed/unknown-or-nonempty-ids/fingerprint fault. Each starts
        // with a real production result accepted by the unchanged input law.
        let rejected = |proof: &DeletedSessionEventAdmissions,
                        first: Option<usize>,
                        result: &Result<SessionEventAdmission, RunError>| {
            assert!(std::panic::catch_unwind(|| check_result(proof, first, "a", result)).is_err());
        };
        let deleted = literal_fixture::<0>(17);
        let result = resolve_deleted_event_admission(&deleted, "a", "a");
        check_result(&deleted, None, "a", &result);
        let mut changed = result.as_ref().unwrap().clone();
        let SessionEventAdmission::Deleted { deleted_revision } = &mut changed else {
            panic!("actual baseline must be Deleted");
        };
        *deleted_revision = SessionRevision(18);
        rejected(&deleted, None, &Ok(changed));
        let mut missing = deleted.clone();
        missing.admissions = None;
        rejected(&missing, None, &result);
        rejected(&deleted, None, &Ok(SessionEventAdmission::NoOp));

        for proof in [literal_fixture::<7>(17), literal_fixture::<27>(17)] {
            let result = resolve_deleted_event_admission(&proof, "a", "a");
            check_result(&proof, Some(0), "a", &result);
            let mut changed = result.as_ref().unwrap().clone();
            match &mut changed {
                SessionEventAdmission::Closed { batch_id }
                | SessionEventAdmission::Accepted { batch_id, .. } => batch_id.push('!'),
                SessionEventAdmission::NoOp | SessionEventAdmission::Deleted { .. } => {
                    panic!("baseline must select a retained batch")
                }
            }
            rejected(&proof, Some(0), &Ok(changed));
            rejected(&proof, Some(0), &Ok(SessionEventAdmission::NoOp));
            for field in 0..2 {
                let mut changed = proof.clone();
                let row = &mut changed.admissions.as_mut().unwrap()[0];
                if field == 0 {
                    row.request_fingerprint.push('!');
                } else {
                    row.closed = !row.closed;
                }
                rejected(&changed, Some(0), &result);
            }
            rejected(&proof, None, &result);
            rejected(&proof, Some(usize::MAX), &result);
            let mut missing = proof.clone();
            missing.admissions = Some(vec![]);
            rejected(&missing, Some(0), &result);
        }

        let mut noop = literal_fixture::<11>(17);
        noop.admissions.as_mut().unwrap()[0].batch_id = None;
        let result = resolve_deleted_event_admission(&noop, "a", "a");
        check_result(&noop, Some(0), "a", &result);
        assert_eq!(result.as_ref().unwrap(), &SessionEventAdmission::NoOp);
        for changed in [
            SessionEventAdmission::Accepted {
                batch_id: "0".into(),
                operation_ids: Some(vec![]),
            },
            SessionEventAdmission::Closed {
                batch_id: "0".into(),
            },
            SessionEventAdmission::Deleted {
                deleted_revision: SessionRevision(17),
            },
        ] {
            rejected(&noop, Some(0), &Ok(changed));
        }
        for field in 0..5 {
            let mut changed = noop.clone();
            let row = &mut changed.admissions.as_mut().unwrap()[0];
            match field {
                0 => row.batch_id = Some("0".into()),
                1 => row.closed = true,
                2 => row.operation_ids = None,
                3 => row.operation_ids = Some(vec!["op".into()]),
                _ => row.request_fingerprint.push('!'),
            }
            rejected(&changed, Some(0), &result);
        }
        rejected(&noop, None, &result);
        rejected(&noop, Some(usize::MAX), &result);
    }

    #[cfg(test)]
    #[test]
    fn deleted_admission_literal_oracle_rejects_each_frame_and_identity_change() {
        // C/E: the four original id shapes cross each single frame-coordinate
        // fault; each wrong id shape/content/order is rejected by BOTH existing
        // frame and result oracles. No production expected-result factory is used.
        let revision = 17;
        for (proof, check) in [
            (
                literal_fixture::<3>(revision),
                check_literal_frame::<3> as fn(&DeletedSessionEventAdmissions, u64),
            ),
            (literal_fixture::<11>(revision), check_literal_frame::<11>),
            (literal_fixture::<19>(revision), check_literal_frame::<19>),
            (literal_fixture::<27>(revision), check_literal_frame::<27>),
        ] {
            check(&proof, revision);
            let baseline = resolve_deleted_event_admission(&proof, "a", "a");
            check_result(&proof, Some(0), "a", &baseline);
            let rejected = |changed: &DeletedSessionEventAdmissions| {
                assert!(std::panic::catch_unwind(|| check(changed, revision)).is_err());
            };
            for field in 0..8 {
                let mut changed = proof.clone();
                match field {
                    0 => changed.deleted_revision = SessionRevision(18),
                    1 => changed.admissions = None,
                    2 => changed.admissions.as_mut().unwrap().clear(),
                    3 => {
                        let rows = changed.admissions.as_mut().unwrap();
                        let duplicate = rows[0].clone();
                        rows.push(duplicate);
                    }
                    _ => {
                        let row = &mut changed.admissions.as_mut().unwrap()[0];
                        match field {
                            4 => row.key.push('!'),
                            5 => row.request_fingerprint.push('!'),
                            6 => row
                                .batch_id
                                .as_mut()
                                .expect("literal fixture retains its batch identity")
                                .push('!'),
                            _ => row.closed = !row.closed,
                        }
                    }
                }
                rejected(&changed);
            }
            let original_ids = &proof.admissions.as_ref().unwrap()[0].operation_ids;
            for ids in [
                None,
                Some(vec![]),
                Some(vec!["x".into()]),
                Some(vec!["x".into(), "y".into()]),
                Some(vec!["y".into(), "x".into()]),
                Some(vec!["changed".into()]),
                Some(vec!["changed".into(), "y".into()]),
                Some(vec!["x".into(), "changed".into()]),
                Some(vec!["x".into(), "y".into(), "z".into()]),
            ] {
                if &ids == original_ids {
                    continue;
                }
                let mut changed_frame = proof.clone();
                changed_frame.admissions.as_mut().unwrap()[0].operation_ids = ids.clone();
                rejected(&changed_frame);
                let mut changed_result = baseline.as_ref().unwrap().clone();
                let SessionEventAdmission::Accepted { operation_ids, .. } = &mut changed_result
                else {
                    panic!("actual baseline must be Accepted");
                };
                *operation_ids = ids;
                assert!(
                    std::panic::catch_unwind(|| {
                        check_result(&proof, Some(0), "a", &Ok(changed_result))
                    })
                    .is_err()
                );
            }
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
