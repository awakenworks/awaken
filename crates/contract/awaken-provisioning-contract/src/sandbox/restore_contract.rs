//! Exact provider-neutral restore identity and physical-target contract.

use super::{
    SandboxCheckpointRef, SandboxError, SandboxHandle, SandboxRestorationEvidence, SandboxSpec,
};

/// Exact provider-neutral identity of one idempotent restore effect.
///
/// The Session aggregate remains the sole durable lifecycle authority. Runtime
/// projects its committed `Restoring { operation, generation, checkpoint }`
/// state into this request so every provider can find or create one physical
/// target after process loss without consulting a Host-local cache.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxRestoreRequest {
    pub workspace_id: String,
    pub session_id: String,
    pub effect_id: String,
    pub generation_id: String,
    pub checkpoint: SandboxCheckpointRef,
}

impl SandboxRestoreRequest {
    /// Secret-free evidence which a completed physical target and its durable
    /// handle must carry.
    #[must_use]
    pub fn evidence(&self, spec: &SandboxSpec) -> SandboxRestorationEvidence {
        SandboxRestorationEvidence::new(
            self.effect_id.clone(),
            self.generation_id.clone(),
            self.checkpoint.id.clone(),
            self.checkpoint.digest.clone(),
            sandbox_spec_security_fingerprint(spec),
            checkpoint_exclusions_fingerprint(&self.checkpoint.excluded_mounts),
        )
    }

    /// Bind this validated restore effect to one exact provider locator.
    /// Ordinary constructors remain legacy-compatible and always emit `None`.
    pub fn bind_handle(
        &self,
        spec: &SandboxSpec,
        mut handle: SandboxHandle,
    ) -> Result<SandboxHandle, SandboxError> {
        self.validate_for_spec(spec)?;
        if handle.sandbox_id != self.session_id {
            return Err(SandboxError::new(
                "restore target handle belongs to a different Session",
            ));
        }
        handle.restoration = Some(self.evidence(spec));
        Ok(handle)
    }

    /// Provider-neutral opaque key for the one physical target owned by this
    /// effect. The full 256-bit digest is never truncated; generation,
    /// checkpoint, specification, and exclusions remain exact fences.
    pub fn physical_target_key(&self, spec: &SandboxSpec) -> Result<String, SandboxError> {
        self.validate_for_spec(spec)?;
        self.evidence(spec).physical_target_key()
    }

    /// Verify that a provider handle is the exact physical projection of this
    /// request and complete frozen specification.
    pub fn verify_handle(
        &self,
        spec: &SandboxSpec,
        handle: &SandboxHandle,
    ) -> Result<(), SandboxError> {
        if handle.sandbox_id != self.session_id {
            return Err(SandboxError::new(
                "restore target handle belongs to a different Session",
            ));
        }
        handle
            .restoration()
            .ok_or_else(|| SandboxError::new("restore target handle has no effect evidence"))?
            .verify(self, spec)
    }

    /// Reject identities which cannot safely name or compare a provider effect.
    pub fn validate(&self) -> Result<(), SandboxError> {
        if [
            self.workspace_id.as_str(),
            self.session_id.as_str(),
            self.effect_id.as_str(),
            self.generation_id.as_str(),
            self.checkpoint.id.as_str(),
            self.checkpoint.digest.as_str(),
        ]
        .into_iter()
        .any(str::is_empty)
        {
            return Err(SandboxError::new(
                "sandbox restore request identity must be non-empty",
            ));
        }
        if awaken_agent_contract::collision_resistant_fingerprint_digest(&self.effect_id).is_none()
        {
            return Err(SandboxError::new(
                "sandbox restoration effect must be canonical blake3 lowercase hex",
            ));
        }
        Ok(())
    }

    /// Bind the durable restore tuple to the complete security-sensitive spec
    /// and validate the provider-owned exclusion evidence without restating the
    /// checkpoint driver's policy. Every declared mount must be excluded, while
    /// the driver may add runtime-owned homes or other independently governed
    /// paths. The exact list is carried and hashed unchanged.
    pub fn validate_for_spec(&self, spec: &SandboxSpec) -> Result<(), SandboxError> {
        self.validate()?;
        if self.session_id != spec.scope {
            return Err(SandboxError::new(
                "sandbox restore request session does not match Sandbox scope",
            ));
        }
        validate_checkpoint_exclusions_for_spec(&self.checkpoint.excluded_mounts, spec)
    }
}

/// Validate provider-owned exclusion evidence without deriving or narrowing
/// it. This is shared by request admission and durable-handle adoption.
pub fn validate_checkpoint_exclusions_for_spec(
    exclusions: &[String],
    spec: &SandboxSpec,
) -> Result<(), SandboxError> {
    if exclusions.iter().any(|path| {
        let path = std::path::Path::new(path);
        !path.is_absolute()
            || path.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::Prefix(_)
                )
            })
    }) {
        return Err(SandboxError::new(
            "checkpoint exclusions must be absolute bounded paths",
        ));
    }
    if spec.mounts.iter().any(|mount| {
        let required = mount.mount_path.trim_end_matches('/');
        !exclusions
            .iter()
            .any(|excluded| excluded.trim_end_matches('/') == required)
    }) {
        return Err(SandboxError::new(
            "checkpoint exclusions do not cover every SandboxSpec mount",
        ));
    }
    Ok(())
}

/// Stable secret-free binding for every current and future SandboxSpec field.
#[must_use]
pub fn sandbox_spec_security_fingerprint(spec: &SandboxSpec) -> String {
    collision_resistant_serialized_fingerprint("awaken-sandbox-spec-security-v1", spec)
}

/// Stable binding for the exact provider-owned exclusion evidence.
#[must_use]
pub fn checkpoint_exclusions_fingerprint(exclusions: &[String]) -> String {
    collision_resistant_serialized_fingerprint(
        "awaken-sandbox-checkpoint-exclusions-v1",
        &exclusions,
    )
}

fn collision_resistant_serialized_fingerprint(
    domain: &str,
    value: &impl serde::Serialize,
) -> String {
    let encoded = serde_json::to_vec(value).expect("restore contract fingerprint serializes");
    awaken_agent_contract::collision_resistant_fingerprint(domain, &[&encoded])
}

/// Completed provider restore result. Construction verifies that the canonical
/// durable handle itself carries the exact request evidence, so a Runtime cannot
/// manufacture a Session receipt from a process-local wrapper alone.
pub struct SandboxRestoreResult<T> {
    target: T,
    evidence: SandboxRestorationEvidence,
}

/// Whether this call created the exact physical target or recovered the target
/// left by an earlier process. Both dispositions name one handle; neither is a
/// completion receipt for checkpoint materialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxRestoreTargetDisposition {
    Created,
    Recovered,
}

/// Provider-substrate result used by a canonical checkpoint decorator while it
/// materializes bytes. It prevents the decorator from calling ordinary create
/// and losing the committed restore identity across process loss.
pub struct SandboxRestoreTarget<T> {
    target: T,
    evidence: SandboxRestorationEvidence,
    disposition: SandboxRestoreTargetDisposition,
}

impl<T> SandboxRestoreTarget<T> {
    pub fn exact(
        request: &SandboxRestoreRequest,
        spec: &SandboxSpec,
        target: T,
        handle: &SandboxHandle,
        disposition: SandboxRestoreTargetDisposition,
    ) -> Result<Self, SandboxError> {
        request.verify_handle(spec, handle)?;
        let evidence = handle
            .restoration()
            .cloned()
            .expect("verified restore handle carries evidence");
        Ok(Self {
            target,
            evidence,
            disposition,
        })
    }

    #[must_use]
    pub const fn target(&self) -> &T {
        &self.target
    }

    #[must_use]
    pub const fn evidence(&self) -> &SandboxRestorationEvidence {
        &self.evidence
    }

    #[must_use]
    pub const fn disposition(&self) -> SandboxRestoreTargetDisposition {
        self.disposition
    }

    #[must_use]
    pub fn into_target(self) -> T {
        self.target
    }

    #[must_use]
    pub fn map_target<U>(self, map: impl FnOnce(T) -> U) -> SandboxRestoreTarget<U> {
        SandboxRestoreTarget {
            target: map(self.target),
            evidence: self.evidence,
            disposition: self.disposition,
        }
    }
}

impl<T> SandboxRestoreResult<T> {
    pub fn complete(
        request: &SandboxRestoreRequest,
        spec: &SandboxSpec,
        target: T,
        handle: &SandboxHandle,
    ) -> Result<Self, SandboxError> {
        request.verify_handle(spec, handle)?;
        let evidence = handle
            .restoration()
            .cloned()
            .expect("verified restore handle carries evidence");
        Ok(Self { target, evidence })
    }

    #[must_use]
    pub const fn evidence(&self) -> &SandboxRestorationEvidence {
        &self.evidence
    }

    #[must_use]
    pub const fn target(&self) -> &T {
        &self.target
    }

    #[must_use]
    pub fn into_target(self) -> T {
        self.target
    }

    #[must_use]
    pub fn map_target<U>(self, map: impl FnOnce(T) -> U) -> SandboxRestoreResult<U> {
        SandboxRestoreResult {
            target: map(self.target),
            evidence: self.evidence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FilesystemContinuity, IsolationClass, MountAccess, MountLifetime, MountRequirement,
        MountSource, NetworkPolicy, ResourceLimits,
    };

    fn spec() -> SandboxSpec {
        SandboxSpec {
            scope: "session-a".into(),
            isolation: IsolationClass::Workdir,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/outputs".into(),
            requests: Default::default(),
            limits: ResourceLimits::default(),
            filesystem_continuity: FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            control_services: Default::default(),
            environment: None,
            command: vec!["agent".into()],
            deny_tool_egress: false,
        }
    }

    fn restore_request() -> SandboxRestoreRequest {
        SandboxRestoreRequest {
            workspace_id: "workspace-a".into(),
            session_id: "session-a".into(),
            effect_id: awaken_agent_contract::collision_resistant_fingerprint(
                "awaken-test-restoration-effect-v1",
                &[b"effect-a"],
            ),
            generation_id: "generation-a".into(),
            checkpoint: SandboxCheckpointRef {
                id: "checkpoint-a".into(),
                format: "opaque-provider-format".into(),
                digest: "digest-a".into(),
                size_bytes: 7,
                created_at_unix_ms: 10,
                expires_at_unix_ms: 20,
                environment_fingerprint: "environment-a".into(),
                base_image_fingerprint: "image-a".into(),
                excluded_mounts: Vec::new(),
                suspend_effect_id: "suspend-a".into(),
            },
        }
    }

    /*
     * Restore-contract cause/effect decision table (R1, R6-R7).
     * Causes: C1 complete non-empty canonical request; C2 handle carries the
     * exact tuple; C3 one tuple field differs; C4 handle omits evidence; C5 one
     * canonical identity field is empty. Effects: E1 construct a completed
     * result/target; E2 reject before publishing completion.
     * Rules: R1=C1+C2=>E1; R6=C1+(C3|C4)=>E2; R7=C5=>E2.
     */
    #[test]
    fn restore_completion_requires_the_exact_canonical_handle_evidence() {
        let request = restore_request();
        let restore_spec = spec();
        let exact = request
            .bind_handle(&restore_spec, SandboxHandle::new("fake", "session-a"))
            .unwrap();
        assert_eq!(
            SandboxRestoreResult::complete(&request, &restore_spec, 7_u8, &exact)
                .unwrap()
                .into_target(),
            7,
            "R1/E1"
        );
        assert!(
            SandboxRestoreTarget::exact(
                &request,
                &restore_spec,
                8_u8,
                &exact,
                SandboxRestoreTargetDisposition::Created,
            )
            .is_ok(),
            "R1/E1"
        );

        let mismatch_request = SandboxRestoreRequest {
            checkpoint: SandboxCheckpointRef {
                digest: "different".into(),
                ..request.checkpoint.clone()
            },
            ..request.clone()
        };
        assert!(
            SandboxRestoreResult::complete(&mismatch_request, &restore_spec, (), &exact).is_err(),
            "R6/E2"
        );
        assert!(
            SandboxRestoreResult::complete(
                &request,
                &restore_spec,
                (),
                &SandboxHandle::new("fake", "session-a"),
            )
            .is_err(),
            "R6/E2"
        );
        let wrong_session =
            request.bind_handle(&restore_spec, SandboxHandle::new("fake", "session-b"));
        assert!(wrong_session.is_err(), "R6/E2");

        let mut invalid = request;
        invalid.effect_id.clear();
        assert!(invalid.validate().is_err(), "R7/E2");
        for malformed in [
            "fnv1a64:0123456789abcdef",
            "blake3:00",
            "blake3:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            invalid.effect_id = malformed.into();
            assert!(invalid.validate().is_err(), "R7/E2 {malformed}");
            assert!(
                invalid.physical_target_key(&restore_spec).is_err(),
                "R7/E2 no physical locator for {malformed}"
            );
        }
    }

    /*
     * Physical-locator/fence table. C1 one canonical authoritative effect; C2
     * generation differs; C3 checkpoint id differs; C4 checkpoint digest
     * differs; C5 full SandboxSpec differs; C6 exact exclusion list differs.
     * Effects: L1 every row selects the same full 256-bit locator; L2 every
     * changed fence produces different exact evidence and must be rejected by
     * observation rather than creating a second target.
     */
    #[test]
    fn physical_target_is_only_the_full_effect_digest_and_all_other_axes_are_fences() {
        let request = restore_request();
        let restore_spec = spec();
        let expected = request.physical_target_key(&restore_spec).unwrap();
        assert_eq!(expected.len(), 64, "L1 full 256-bit lowercase hex");
        assert_eq!(
            Some(expected.as_str()),
            awaken_agent_contract::collision_resistant_fingerprint_digest(&request.effect_id),
            "L1 exact authoritative effect digest"
        );

        let mut variants = Vec::new();
        let mut generation = request.clone();
        generation.generation_id.push_str("-other");
        variants.push(("C2", generation, restore_spec.clone()));
        let mut checkpoint_id = request.clone();
        checkpoint_id.checkpoint.id.push_str("-other");
        variants.push(("C3", checkpoint_id, restore_spec.clone()));
        let mut checkpoint_digest = request.clone();
        checkpoint_digest.checkpoint.digest.push_str("-other");
        variants.push(("C4", checkpoint_digest, restore_spec.clone()));
        let mut changed_spec = restore_spec.clone();
        changed_spec.deny_tool_egress = true;
        variants.push(("C5", request.clone(), changed_spec));
        let mut exclusions = request.clone();
        exclusions
            .checkpoint
            .excluded_mounts
            .push("/runtime-owned-home".into());
        variants.push(("C6", exclusions, restore_spec.clone()));

        let baseline_evidence = request.evidence(&restore_spec);
        for (cause, changed_request, changed_spec) in variants {
            assert_eq!(
                changed_request.physical_target_key(&changed_spec).unwrap(),
                expected,
                "{cause}/L1"
            );
            assert_ne!(
                changed_request.evidence(&changed_spec),
                baseline_evidence,
                "{cause}/L2"
            );
        }
    }

    /*
     * Exclusion ownership rules: C1 the checkpoint driver records every
     * SandboxSpec mount plus its own runtime homes; C2 one declared mount is
     * missing; C3 the provider reorders or narrows persisted evidence.
     * Effects: E1 accept the opaque exact superset; E2 reject before physical
     * acquisition; E3 produce a different evidence hash. Rules X1=C1=>E1,
     * X2=C2=>E2, X3=C3=>E3.
     */
    #[test]
    fn restore_exclusions_preserve_provider_owned_exact_superset() {
        let mut restore_spec = spec();
        restore_spec.mounts.push(MountRequirement {
            mount_id: "input-a".into(),
            source: MountSource::Inline {
                contents: "value".into(),
            },
            mount_path: "/workspace/input".into(),
            access: MountAccess::ReadOnly,
            required: true,
            lifetime: MountLifetime::Session,
        });
        let mut request = restore_request();
        request.checkpoint.excluded_mounts =
            vec!["/home/awaken/.config".into(), "/workspace/input".into()];
        assert!(request.validate_for_spec(&restore_spec).is_ok(), "X1/E1");

        let exact_hash = request
            .evidence(&restore_spec)
            .checkpoint_exclusions_fingerprint()
            .to_owned();
        assert!(
            awaken_agent_contract::collision_resistant_fingerprint_digest(
                request.evidence(&restore_spec).sandbox_spec_fingerprint()
            )
            .is_some(),
            "X1 collision-resistant full spec"
        );
        assert!(
            awaken_agent_contract::collision_resistant_fingerprint_digest(&exact_hash).is_some(),
            "X1 collision-resistant exact exclusions"
        );
        let mut reordered = request.clone();
        reordered.checkpoint.excluded_mounts.reverse();
        assert_ne!(
            reordered
                .evidence(&restore_spec)
                .checkpoint_exclusions_fingerprint(),
            exact_hash,
            "X3/E3"
        );
        request.checkpoint.excluded_mounts.remove(1);
        assert!(request.validate_for_spec(&restore_spec).is_err(), "X2/E2");
    }
}
