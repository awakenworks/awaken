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
