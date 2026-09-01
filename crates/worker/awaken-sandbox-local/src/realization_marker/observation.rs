use super::*;

fn incompatible(reason: impl ToString) -> pc::SandboxObservation {
    pc::SandboxObservation::Incompatible {
        reason: reason.to_string(),
    }
}

pub(super) fn validate_handle_marker(
    marker: &RealizationMarker,
    fingerprint: &pc::SandboxRealizationFingerprint,
    handle_fence: &pc::SandboxEffectFence,
    incarnation: &str,
) -> Result<(), pc::SandboxError> {
    handle_fence
        .validate_identity()
        .map_err(|_| err("sandbox handle effect fence has incomplete identity"))?;
    if marker.fingerprint != *fingerprint || marker.physical_incarnation != incarnation {
        return Err(err(
            "sandbox handle does not identify the current physical realization",
        ));
    }
    let expected_handle_fence = match marker.phase {
        RealizationPhase::Creating | RealizationPhase::Recreating | RealizationPhase::Ready => {
            &marker.effect_fence
        }
        RealizationPhase::Removing | RealizationPhase::Removed => {
            &marker
                .terminal_source
                .as_ref()
                .ok_or_else(|| err("terminal sandbox marker has no immutable source evidence"))?
                .realization_effect_fence
        }
    };
    if !expected_handle_fence.same_effect_identity(handle_fence) {
        return Err(err(
            "sandbox handle effect does not identify the current realization",
        ));
    }
    Ok(())
}

/// Effect-free observation of one exact durable handle.
pub(crate) fn observe_adoption(
    root: &Path,
    fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    handle_fence: Option<&pc::SandboxEffectFence>,
    physical_incarnation: Option<&str>,
    current_fence: Option<&pc::SandboxEffectFence>,
) -> Result<pc::SandboxObservation, pc::SandboxError> {
    observe_adoption_inner(
        root,
        fingerprint,
        handle_fence,
        physical_incarnation,
        current_fence,
        None,
    )
}

fn observe_adoption_inner(
    root: &Path,
    fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    handle_fence: Option<&pc::SandboxEffectFence>,
    physical_incarnation: Option<&str>,
    current_fence: Option<&pc::SandboxEffectFence>,
    lock: Option<&awaken_sandbox_fs::ExclusiveFileLock>,
) -> Result<pc::SandboxObservation, pc::SandboxError> {
    let current = match (fingerprint, handle_fence, physical_incarnation) {
        (None, None, None) => None,
        (Some(fingerprint), Some(handle_fence), Some(incarnation))
            if !incarnation.trim().is_empty() =>
        {
            Some((fingerprint, handle_fence, incarnation))
        }
        _ => {
            return Ok(incompatible(
                "filesystem sandbox handle has incomplete current evidence",
            ));
        }
    };
    if let Some(current_fence) = current_fence {
        validate_live_effect_fence(current_fence)?;
    }

    let marker = match match lock {
        Some(lock) => read_marker_locked(root, lock),
        None => read_marker(root),
    } {
        Ok(marker) => marker,
        Err(error) => return Ok(incompatible(error)),
    };
    let Some((fingerprint, handle_fence, incarnation)) = current else {
        if marker.is_some() {
            return Ok(incompatible(
                "legacy sandbox handle cannot claim a current realization marker",
            ));
        }
        let root_entry = match lock {
            Some(lock) => classify_locked(lock, root_leaf(root)?)?,
            None => classify(root)?,
        };
        return match root_entry {
            PathEntry::Directory(_) => Ok(pc::SandboxObservation::Ready),
            PathEntry::Absent => Ok(pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: None,
            }),
            _ => Ok(incompatible(
                "legacy sandbox root is a file, symlink, or special entry",
            )),
        };
    };
    let Some(marker) = marker else {
        return Ok(incompatible(
            "current sandbox handle has no realization marker",
        ));
    };
    if let Err(error) = validate_handle_marker(&marker, fingerprint, handle_fence, incarnation) {
        return Ok(incompatible(error));
    }
    if let Some(current_fence) = current_fence
        && !marker.effect_fence.authorizes_successor(current_fence)
    {
        return Ok(incompatible(
            "sandbox realization is owned by a newer or foreign effect fence",
        ));
    }

    let owned = match match lock {
        Some(lock) => owned_realization_path_locked(root, lock, &marker),
        None => owned_realization_path(root, &marker),
    } {
        Ok(owned) => owned,
        Err(error) => return Ok(incompatible(error)),
    };
    match marker.phase {
        RealizationPhase::Creating | RealizationPhase::Recreating => {
            Ok(pc::SandboxObservation::Provisioning)
        }
        RealizationPhase::Ready => match owned {
            OwnedRealizationPath::Exact { path, .. } if path == root => {
                Ok(pc::SandboxObservation::Ready)
            }
            OwnedRealizationPath::Absent => Ok(pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: None,
            }),
            _ => Ok(incompatible(
                "ready sandbox is still held at its private stage",
            )),
        },
        RealizationPhase::Removing => match owned {
            OwnedRealizationPath::Exact { .. } | OwnedRealizationPath::Absent => {
                if marker.disposal_authorization.is_some() {
                    Ok(pc::SandboxObservation::Disposing {
                        physical_incarnation: marker.physical_incarnation,
                    })
                } else {
                    Ok(pc::SandboxObservation::Terminal {
                        physical_incarnation: marker.physical_incarnation,
                    })
                }
            }
        },
        RealizationPhase::Removed => match owned {
            OwnedRealizationPath::Absent => Ok(pc::SandboxObservation::DefinitivelyUnavailable {
                // The durable tombstone remains provider-private evidence for
                // response-loss; there is no live physical incarnation to adopt.
                physical_incarnation: None,
            }),
            OwnedRealizationPath::Exact { .. } => Ok(incompatible(
                "removed sandbox tombstone has a physical root",
            )),
        },
    }
}

/// Revalidate one handle under the stable mutation lock and return exact live
/// evidence for a concrete Local/Namespace sandbox object.
pub(crate) fn verify_adoption(
    root: &Path,
    fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    handle_fence: Option<&pc::SandboxEffectFence>,
    physical_incarnation: Option<&str>,
    current_fence: Option<&pc::SandboxEffectFence>,
) -> Result<Option<(RealizationEvidence, RealizationCompletionReceipt)>, pc::SandboxError> {
    let lock = acquire(root)?;
    match observe_adoption_inner(
        root,
        fingerprint,
        handle_fence,
        physical_incarnation,
        current_fence,
        Some(&lock),
    )? {
        pc::SandboxObservation::Ready => match read_marker_locked(root, &lock)? {
            Some(marker) => {
                let completion = marker.completion.clone().ok_or_else(|| {
                    err("Ready filesystem realization has no exact completion receipt")
                })?;
                Ok(Some((evidence(&marker)?, completion)))
            }
            None => Ok(None),
        },
        pc::SandboxObservation::Provisioning => Err(err("sandbox realization did not reach Ready")),
        pc::SandboxObservation::DefinitivelyUnavailable { .. }
        | pc::SandboxObservation::Terminal { .. }
        | pc::SandboxObservation::Disposing { .. } => Err(err(
            "sandbox realization is terminal or physically unavailable",
        )),
        pc::SandboxObservation::Incompatible { reason } => Err(err(reason)),
    }
}

pub(super) fn validate_absent_terminal_source(
    fingerprint: &pc::SandboxRealizationFingerprint,
    handle: Option<&RebuildSource<'_>>,
    expected_effect_fence: Option<&pc::SandboxEffectFence>,
    terminal_effect_fence: &pc::SandboxEffectFence,
) -> Result<(TerminalSourceRecord, Option<String>), pc::SandboxError> {
    let (source_fence, incarnation) = match handle {
        Some(source) => {
            source.effect_fence.validate_identity()?;
            if source.fingerprint != fingerprint || source.physical_incarnation.trim().is_empty() {
                return Err(err(
                    "terminal handle does not bind the absent filesystem realization",
                ));
            }
            (
                source.effect_fence,
                Some(source.physical_incarnation.to_owned()),
            )
        }
        None => {
            let expected = expected_effect_fence.ok_or_else(|| {
                err("handle-free terminal cleanup requires its exact restore effect fence")
            })?;
            (expected, None)
        }
    };
    if !source_fence.authorizes_successor(terminal_effect_fence)
        || expected_effect_fence
            .is_some_and(|expected| !source_fence.same_effect_identity(expected))
        || expected_effect_fence
            .is_some_and(|expected| !expected.authorizes_successor(terminal_effect_fence))
    {
        return Err(err(
            "terminal fence does not authorize the absent realization evidence",
        ));
    }
    Ok((
        TerminalSourceRecord {
            realization_effect_fence: source_fence.clone(),
            expected_effect_fence: expected_effect_fence.cloned(),
        },
        incarnation,
    ))
}

/// Validate one terminal request against the immutable physical source recorded
/// by the marker. Both the effect-free preflight and the mutating takeover use
/// this function, so phase/source interpretation cannot diverge between them.
pub(super) fn validate_terminal_source(
    marker: &RealizationMarker,
    fingerprint: &pc::SandboxRealizationFingerprint,
    handle: Option<&RebuildSource<'_>>,
    expected_effect_fence: Option<&pc::SandboxEffectFence>,
    terminal_effect_fence: &pc::SandboxEffectFence,
) -> Result<TerminalSourceRecord, pc::SandboxError> {
    if marker.fingerprint != *fingerprint {
        return Err(err(
            "terminal cleanup specification does not identify the realization marker",
        ));
    }
    let terminal_source = match marker.phase {
        RealizationPhase::Removing | RealizationPhase::Removed => {
            let recorded = marker.terminal_source.clone().ok_or_else(|| {
                err("terminal realization marker has no immutable source evidence")
            })?;
            match handle {
                Some(source) => {
                    validate_handle_marker(
                        marker,
                        source.fingerprint,
                        source.effect_fence,
                        source.physical_incarnation,
                    )?;
                    if !recorded
                        .realization_effect_fence
                        .same_effect_identity(source.effect_fence)
                    {
                        return Err(err(
                            "terminal handle conflicts with the recorded physical source",
                        ));
                    }
                }
                None => {
                    let expected = expected_effect_fence.ok_or_else(|| {
                        err("handle-free terminal replay requires its exact restore effect")
                    })?;
                    if !recorded
                        .realization_effect_fence
                        .same_effect_identity(expected)
                    {
                        return Err(err(
                            "handle-free terminal replay does not identify the restore source",
                        ));
                    }
                }
            }
            let expected_matches = match (
                recorded.expected_effect_fence.as_ref(),
                expected_effect_fence,
            ) {
                (None, None) => true,
                (Some(recorded), Some(expected)) => recorded.same_effect_identity(expected),
                _ => false,
            };
            if !expected_matches {
                return Err(err(
                    "terminal replay changed the expected in-flight operation fence",
                ));
            }
            if marker.phase == RealizationPhase::Removing
                && !marker
                    .effect_fence
                    .authorizes_successor(terminal_effect_fence)
            {
                return Err(err("terminal cleanup fence is stale or foreign"));
            }
            recorded
        }
        RealizationPhase::Creating | RealizationPhase::Recreating | RealizationPhase::Ready => {
            let realization_effect_fence = match handle {
                Some(source) => {
                    validate_handle_marker(
                        marker,
                        source.fingerprint,
                        source.effect_fence,
                        source.physical_incarnation,
                    )?;
                    source.effect_fence.clone()
                }
                None => {
                    let expected = expected_effect_fence.ok_or_else(|| {
                        err("handle-free terminal cleanup requires its exact restore effect")
                    })?;
                    if !marker.effect_fence.same_effect_identity(expected)
                        || !marker
                            .effect_fence
                            .authorizes_successor(terminal_effect_fence)
                    {
                        return Err(err(
                            "handle-free terminal cleanup does not identify an authorized in-flight restore",
                        ));
                    }
                    expected.clone()
                }
            };
            if let Some(expected) = expected_effect_fence
                && !realization_effect_fence.same_effect_identity(expected)
            {
                return Err(err(
                    "expected operation fence does not exactly identify the physical source effect",
                ));
            }
            TerminalSourceRecord {
                realization_effect_fence,
                expected_effect_fence: expected_effect_fence.cloned(),
            }
        }
    };

    if !terminal_source
        .realization_effect_fence
        .authorizes_successor(terminal_effect_fence)
        || terminal_source
            .expected_effect_fence
            .as_ref()
            .is_some_and(|expected| !expected.authorizes_successor(terminal_effect_fence))
    {
        return Err(err(
            "terminal fence does not authorize the exact physical or in-flight source",
        ));
    }
    Ok(terminal_source)
}

/// Typed, effect-free terminal preflight. `Receipt` means a Ready participant
/// or its Removing response-loss replay retains one exact completion receipt;
/// `Incomplete` means the exact attempt exists but never published Ready;
/// `Closed` means the marker is absent or durably Removed. Providers therefore
/// never confuse an in-flight Memory attempt with an idempotent terminal replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TerminalReceiptObservation {
    Receipt(RealizationCompletionReceipt),
    Incomplete,
    Closed,
}

/// Compare the effect-free observation with the marker state under the later
/// takeover lock. This token is deliberately phase-aware: receipt publication
/// or a late creator after an absent observation must force a zero-write retry
/// instead of being reinterpreted by the terminal mutator.
pub(super) fn validate_terminal_preflight(
    expected: Option<&TerminalReceiptObservation>,
    marker: Option<&RealizationMarker>,
) -> Result<(), pc::SandboxError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let matches = match (expected, marker) {
        (TerminalReceiptObservation::Receipt(receipt), Some(marker)) => {
            matches!(
                marker.phase,
                RealizationPhase::Ready | RealizationPhase::Removing
            ) && marker.completion.as_ref() == Some(receipt)
        }
        (TerminalReceiptObservation::Incomplete, Some(marker)) => {
            matches!(
                marker.phase,
                RealizationPhase::Creating
                    | RealizationPhase::Recreating
                    | RealizationPhase::Removing
            ) && marker.completion.is_none()
        }
        (TerminalReceiptObservation::Closed, None) => true,
        (TerminalReceiptObservation::Closed, Some(marker)) => {
            marker.phase == RealizationPhase::Removed
        }
        _ => false,
    };
    if matches {
        Ok(())
    } else {
        Err(err(
            "terminal marker changed after its effect-free phase/receipt preflight",
        ))
    }
}

pub(crate) fn observe_terminal_receipt(
    root: &Path,
    fingerprint: &pc::SandboxRealizationFingerprint,
    handle: Option<&RebuildSource<'_>>,
    expected_effect_fence: Option<&pc::SandboxEffectFence>,
    terminal_effect_fence: &pc::SandboxEffectFence,
) -> Result<TerminalReceiptObservation, pc::SandboxError> {
    if let Some(expected) = expected_effect_fence {
        expected.validate_identity()?;
    }
    validate_live_effect_fence(terminal_effect_fence)?;
    let lock = acquire(root)?;
    let Some(marker) = read_marker_locked(root, &lock)? else {
        validate_absent_terminal_source(
            fingerprint,
            handle,
            expected_effect_fence,
            terminal_effect_fence,
        )?;
        return Ok(TerminalReceiptObservation::Closed);
    };
    validate_terminal_source(
        &marker,
        fingerprint,
        handle,
        expected_effect_fence,
        terminal_effect_fence,
    )?;
    let owned = owned_realization_path_locked(root, &lock, &marker)?;
    match marker.phase {
        RealizationPhase::Creating | RealizationPhase::Recreating => {
            Ok(TerminalReceiptObservation::Incomplete)
        }
        RealizationPhase::Ready => match owned {
            OwnedRealizationPath::Exact { path, .. } if path == root => marker
                .completion
                .clone()
                .ok_or_else(|| err("Ready filesystem realization has no exact completion receipt"))
                .map(TerminalReceiptObservation::Receipt),
            _ => Err(err(
                "Ready terminal preflight has no exact published Sandbox root",
            )),
        },
        RealizationPhase::Removing => marker
            .completion
            .clone()
            .map_or(Ok(TerminalReceiptObservation::Incomplete), |receipt| {
                Ok(TerminalReceiptObservation::Receipt(receipt))
            }),
        RealizationPhase::Removed => match owned {
            OwnedRealizationPath::Absent => Ok(TerminalReceiptObservation::Closed),
            OwnedRealizationPath::Exact { .. } => Err(err(
                "Removed terminal preflight still has a physical Sandbox participant",
            )),
        },
    }
}
