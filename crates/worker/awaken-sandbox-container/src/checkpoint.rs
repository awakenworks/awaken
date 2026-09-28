// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: Static owner: current-authority access and checkpoint participant transitions for the
// @formal reason: existing Container runtime. Dynamic path: acquire before begin, preserve original upload
// @formal reason: identity across terminal takeover, and delegate receipt validation to
// @formal reason: SandboxCheckpointUpload; rejected or decoded conflicting evidence grants no new
// @formal reason: authority. Adjacent participant tables are executable evidence, not a direct proof of
// @formal reason: external effects.
//! Physical checkpoint participant access. Archive and object custody stay at
//! the calling codec/store; Session remains the only lifecycle authority.

use awaken_provisioning_contract as pc;
use pc::FenceToken as _;

/// Immutable original upload evidence and an independently current grant.
/// The source fence may have expired; only `authority` grants an effect.
#[derive(Clone, Copy)]
pub struct ContainerCheckpointAccess<'a> {
    pub source_effect_fence: &'a pc::SandboxEffectFence,
    pub effect_fence: &'a pc::SandboxEffectFence,
    pub request_fingerprint: &'a str,
    pub authority: &'a dyn pc::CurrentExecutionAuthority,
}

impl ContainerCheckpointAccess<'_> {
    pub(crate) fn admit(&self) -> Result<(), pc::SandboxError> {
        self.source_effect_fence.validate_identity()?;
        if self.request_fingerprint.trim().is_empty()
            || !self
                .source_effect_fence
                .authorizes_successor(self.effect_fence)
        {
            return Err(pc::SandboxError::invalid(
                "invalid container checkpoint access",
            ));
        }
        pc::with_current_sandbox_effect(self.effect_fence, self.authority, |_| Ok(()))
    }

    #[cfg(feature = "k8s")]
    pub(crate) fn is_terminal(&self) -> bool {
        self.source_effect_fence.operation_id != self.effect_fence.operation_id
    }
}

/// One physical participant, not an object-storage or Session command. Acquire
/// is also the current-authority/physical-identity check before and after I/O.
#[derive(Clone, Copy)]
pub enum ContainerCheckpointMutation<'a> {
    Acquire,
    Begin(&'a pc::SandboxCheckpointUpload),
    Complete(&'a pc::SandboxCheckpointRef),
}

#[cfg(feature = "k8s")]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContainerCheckpointParticipant {
    source_effect_fence: pc::SandboxEffectFence,
    request_fingerprint: String,
    terminal: bool,
    upload: Option<pc::SandboxCheckpointUpload>,
}

#[cfg(feature = "k8s")]
impl ContainerCheckpointParticipant {
    pub(crate) fn apply(
        previous: Option<&Self>,
        access: ContainerCheckpointAccess<'_>,
        mutation: ContainerCheckpointMutation<'_>,
    ) -> Result<Self, pc::SandboxError> {
        access.admit()?;
        let mut next = match previous {
            Some(previous) => {
                if !previous
                    .source_effect_fence
                    .same_effect_identity(access.source_effect_fence)
                    || previous.request_fingerprint != access.request_fingerprint
                    || (previous.terminal && !access.is_terminal())
                {
                    return Err(pc::SandboxError::conflict(
                        "container checkpoint participant conflicts with its original upload or terminal takeover",
                    ));
                }
                if let Some(upload) = &previous.upload
                    && let Some(reference) = upload.completed_for(
                        access.request_fingerprint,
                        &access.source_effect_fence.operation_id,
                    )?
                {
                    // Decode is not proof: revalidate a stored complete
                    // receipt against its own original pending evidence.
                    upload.clone().complete(
                        &access.source_effect_fence.operation_id,
                        access.request_fingerprint,
                        &reference,
                    )?;
                }
                previous.clone()
            }
            None if matches!(mutation, ContainerCheckpointMutation::Acquire) => Self {
                source_effect_fence: access.source_effect_fence.clone(),
                request_fingerprint: access.request_fingerprint.to_owned(),
                terminal: access.is_terminal(),
                upload: None,
            },
            None => {
                return Err(pc::SandboxError::not_found(
                    "container checkpoint participant was not acquired",
                ));
            }
        };
        next.terminal |= access.is_terminal();
        match mutation {
            ContainerCheckpointMutation::Acquire => {}
            ContainerCheckpointMutation::Begin(pending) => {
                if pending.operation_id() != Some(access.source_effect_fence.operation_id.as_str())
                    || pending.request_fingerprint() != access.request_fingerprint
                    || pending.reference().is_some()
                {
                    return Err(pc::SandboxError::conflict(
                        "container checkpoint pending evidence differs from its admitted request",
                    ));
                }
                match &next.upload {
                    Some(recorded) if !recorded.matches_pending(pending) => {
                        return Err(pc::SandboxError::conflict(
                            "container checkpoint replay changed snapshot bytes",
                        ));
                    }
                    Some(_) => {}
                    None => next.upload = Some(pending.clone()),
                }
            }
            ContainerCheckpointMutation::Complete(reference) => {
                next.upload
                    .as_mut()
                    .ok_or_else(|| {
                        pc::SandboxError::not_found("container checkpoint has no pending upload")
                    })?
                    .complete(
                        &access.source_effect_fence.operation_id,
                        access.request_fingerprint,
                        reference,
                    )?;
            }
        }
        Ok(next)
    }

    pub(crate) fn upload(&self) -> Option<pc::SandboxCheckpointUpload> {
        self.upload.clone()
    }
}

#[cfg(all(test, feature = "k8s"))]
pub(crate) mod tests {
    use super::*;
    use pc::test_support::EffectAuthority;

    pub(crate) fn receipt() -> pc::SandboxCheckpointRef {
        pc::SandboxCheckpointRef {
            id: "object".into(),
            format: "fixture".into(),
            digest: "digest".into(),
            size_bytes: 4,
            created_at_unix_ms: 1,
            expires_at_unix_ms: 2,
            environment_fingerprint: "environment".into(),
            base_image_fingerprint: "image".into(),
            excluded_mounts: vec![],
            suspend_effect_id: "suspend".into(),
        }
    }

    #[test]
    fn participant_decision_table_keeps_terminal_takeover_irreversible() {
        // CP graph: C1 absent/acquired/pending/complete record; C2 ordinary or
        // terminal caller; C3 prior terminal takeover; C4 exact/foreign request;
        // C5 current/denied grant; C6 acquire/begin/complete. CP1 exact+current
        // admits Acquire; Begin requires acquisition; Complete requires pending.
        // CP2 prior terminal rejects ordinary callers, even at SAME lease epoch.
        // CP3 mismatched request/denied authority rejects every mutation.
        // CP4 every rejection leaves the input record byte-for-byte unchanged.
        let source = pc::SandboxEffectFence::new("suspend", "owner", "boot", 2, 100).unwrap();
        let terminal = pc::SandboxEffectFence::new("terminal", "owner", "boot", 2, 100).unwrap();
        let pending =
            pc::SandboxCheckpointUpload::pending("suspend".into(), "request", "digest", 4);
        let reference = receipt();
        for stage in 0..4 {
            for prior_terminal in [false, true] {
                let stored = if stage == 0 {
                    None
                } else {
                    let mut upload = (stage >= 2).then(|| pending.clone());
                    if stage == 3 {
                        upload
                            .as_mut()
                            .unwrap()
                            .complete("suspend", "request", &reference)
                            .unwrap();
                    }
                    Some(ContainerCheckpointParticipant {
                        source_effect_fence: source.clone(),
                        request_fingerprint: "request".into(),
                        terminal: prior_terminal,
                        upload,
                    })
                };
                for current_terminal in [false, true] {
                    for exact_request in [false, true] {
                        for current in [false, true] {
                            let active = if current_terminal { &terminal } else { &source };
                            let authority = if current {
                                EffectAuthority::new(active)
                            } else {
                                EffectAuthority::denied()
                            };
                            let access = ContainerCheckpointAccess {
                                source_effect_fence: &source,
                                effect_fence: active,
                                request_fingerprint: if exact_request {
                                    "request"
                                } else {
                                    "foreign"
                                },
                                authority: &authority,
                            };
                            for (index, mutation) in [
                                ContainerCheckpointMutation::Acquire,
                                ContainerCheckpointMutation::Begin(&pending),
                                ContainerCheckpointMutation::Complete(&reference),
                            ]
                            .into_iter()
                            .enumerate()
                            {
                                let before = serde_json::to_string(&stored).unwrap();
                                let allowed = current
                                    && (exact_request || stage == 0)
                                    && !(stage > 0 && prior_terminal && !current_terminal)
                                    && match index {
                                        0 => true,
                                        1 => stage > 0 && exact_request,
                                        2 => stage >= 2,
                                        _ => unreachable!(),
                                    };
                                let result = ContainerCheckpointParticipant::apply(
                                    stored.as_ref(),
                                    access,
                                    mutation,
                                );
                                assert_eq!(
                                    result.is_ok(),
                                    allowed,
                                    "CP {stage}/{prior_terminal}/{current_terminal}/{exact_request}/{current}/{index}: {result:?}"
                                );
                                assert_eq!(serde_json::to_string(&stored).unwrap(), before, "CP4");
                                if let Ok(next) = result {
                                    assert_eq!(
                                        next.terminal,
                                        current_terminal || (stage > 0 && prior_terminal)
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn decoded_receipt_and_pending_identity_cannot_bypass_shared_evidence_checks() {
        // CI1 corrupt stored operation/request/digest/size/suspend-effect ->
        // reject Acquire, not a cached success. CI2 unknown fields -> decode
        // fails. CI3 missing/foreign source identity or regressing lease ->
        // rejects before participant mutation. Shared receipt equality itself
        // is exhaustively covered at SandboxCheckpointUpload, not duplicated.
        let source = pc::SandboxEffectFence::new("suspend", "owner", "boot", 2, 100).unwrap();
        let authority = EffectAuthority::new(&source);
        let access = ContainerCheckpointAccess {
            source_effect_fence: &source,
            effect_fence: &source,
            request_fingerprint: "request",
            authority: &authority,
        };
        let mut upload =
            pc::SandboxCheckpointUpload::pending("suspend".into(), "request", "digest", 4);
        upload.complete("suspend", "request", &receipt()).unwrap();
        let stored = ContainerCheckpointParticipant {
            source_effect_fence: source.clone(),
            request_fingerprint: "request".into(),
            terminal: false,
            upload: Some(upload),
        };
        for pointer in [
            "/upload/operation_id",
            "/upload/request_fingerprint",
            "/upload/snapshot_digest",
            "/upload/snapshot_size_bytes",
            "/upload/reference/suspend_effect_id",
        ] {
            let mut wire = serde_json::to_value(&stored).unwrap();
            *wire.pointer_mut(pointer).unwrap() = if pointer.ends_with("size_bytes") {
                5.into()
            } else {
                "foreign".into()
            };
            let corrupt = serde_json::from_value(wire).unwrap();
            assert!(
                ContainerCheckpointParticipant::apply(
                    Some(&corrupt),
                    access,
                    ContainerCheckpointMutation::Acquire
                )
                .is_err(),
                "CI1/{pointer}"
            );
        }
        let mut wire = serde_json::to_value(&stored).unwrap();
        wire["second_owner"] = true.into();
        assert!(
            serde_json::from_value::<ContainerCheckpointParticipant>(wire).is_err(),
            "CI2"
        );
        for axis in ["operation", "owner", "boot", "epoch"] {
            let mut changed = source.clone();
            match axis {
                "operation" => changed.operation_id.push('x'),
                "owner" => changed.owner.push('x'),
                "boot" => changed.runtime_incarnation.push('x'),
                "epoch" => changed.epoch += 1,
                _ => unreachable!(),
            }
            assert!(
                ContainerCheckpointParticipant::apply(
                    Some(&stored),
                    ContainerCheckpointAccess {
                        source_effect_fence: &changed,
                        ..access
                    },
                    ContainerCheckpointMutation::Acquire
                )
                .is_err(),
                "CI3/{axis}"
            );
        }
    }
}
