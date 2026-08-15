//! Pure validation of the identities duplicated around an executable snapshot.
//!
//! The registration envelope and immutable snapshot deliberately repeat the
//! Agent, source revision, and content fingerprint at trust boundaries. This
//! module gives all adapters one exact, representation-independent validation
//! rule before accepting the publication pin.

/// The canonical pin emitted after all duplicated identity axes agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedSnapshotPin<T> {
    pub workspace: T,
    pub agent: T,
    pub source_revision: u64,
    pub fingerprint: T,
}

/// Identity fields carried by the registration envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotRegistrationIdentity<T> {
    pub workspace: Option<T>,
    pub agent: Option<T>,
    pub source_revision: u64,
}

/// Identity fields carried by the executable snapshot and its metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutableSnapshotIdentity<T> {
    pub root_agent: T,
    pub published_metadata: bool,
    pub source_agent: T,
    pub source_revision: u64,
    pub envelope_fingerprint: Option<T>,
    pub resolved_fingerprint: Option<T>,
    pub metadata_fingerprint: Option<T>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotPinError {
    #[error("workspace_id must not be empty")]
    MissingWorkspace,
    #[error("agent_id must not be empty")]
    MissingAgent,
    #[error("source_revision must be non-zero")]
    MissingSourceRevision,
    #[error("snapshot root Agent does not match agent_id")]
    SnapshotRootMismatch,
    #[error("registered publication must carry source metadata")]
    MissingPublishedMetadata,
    #[error("snapshot source Agent does not match agent_id")]
    SourceAgentMismatch,
    #[error("snapshot source revision does not match registration revision")]
    SourceRevisionMismatch,
    #[error("snapshot fingerprint must not be empty")]
    MissingFingerprint,
    #[error("snapshot fingerprints must be identical")]
    FingerprintMismatch,
}

/// Validate and collapse all duplicate publication identity fields into one pin.
pub fn published_snapshot_pin<T: Copy + Eq>(
    registration: SnapshotRegistrationIdentity<T>,
    snapshot: ExecutableSnapshotIdentity<T>,
) -> Result<PublishedSnapshotPin<T>, SnapshotPinError> {
    let workspace = registration
        .workspace
        .ok_or(SnapshotPinError::MissingWorkspace)?;
    let agent = registration.agent.ok_or(SnapshotPinError::MissingAgent)?;
    if registration.source_revision == 0 {
        return Err(SnapshotPinError::MissingSourceRevision);
    }
    if snapshot.root_agent != agent {
        return Err(SnapshotPinError::SnapshotRootMismatch);
    }
    if !snapshot.published_metadata {
        return Err(SnapshotPinError::MissingPublishedMetadata);
    }
    if snapshot.source_agent != agent {
        return Err(SnapshotPinError::SourceAgentMismatch);
    }
    if snapshot.source_revision != registration.source_revision {
        return Err(SnapshotPinError::SourceRevisionMismatch);
    }

    let fingerprint = snapshot
        .envelope_fingerprint
        .ok_or(SnapshotPinError::MissingFingerprint)?;
    let resolved_fingerprint = snapshot
        .resolved_fingerprint
        .ok_or(SnapshotPinError::MissingFingerprint)?;
    let metadata_fingerprint = snapshot
        .metadata_fingerprint
        .ok_or(SnapshotPinError::MissingFingerprint)?;
    if resolved_fingerprint != fingerprint || metadata_fingerprint != fingerprint {
        return Err(SnapshotPinError::FingerprintMismatch);
    }

    Ok(PublishedSnapshotPin {
        workspace,
        agent,
        source_revision: registration.source_revision,
        fingerprint,
    })
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    fn executable_snapshot_pin_is_complete_exact_and_non_mixing() {
        let registration: SnapshotRegistrationIdentity<u8> = SnapshotRegistrationIdentity {
            workspace: kani::any(),
            agent: kani::any(),
            source_revision: kani::any(),
        };
        let snapshot = ExecutableSnapshotIdentity {
            root_agent: kani::any(),
            published_metadata: kani::any(),
            source_agent: kani::any(),
            source_revision: kani::any(),
            envelope_fingerprint: kani::any(),
            resolved_fingerprint: kani::any(),
            metadata_fingerprint: kani::any(),
        };

        let expected = registration.workspace.is_some()
            && registration.agent.is_some()
            && registration.source_revision > 0
            && registration.agent == Some(snapshot.root_agent)
            && snapshot.published_metadata
            && registration.agent == Some(snapshot.source_agent)
            && registration.source_revision == snapshot.source_revision
            && snapshot.envelope_fingerprint.is_some()
            && snapshot.envelope_fingerprint == snapshot.resolved_fingerprint
            && snapshot.envelope_fingerprint == snapshot.metadata_fingerprint;
        let result = published_snapshot_pin(registration, snapshot);
        assert_eq!(result.is_ok(), expected);

        if let Ok(pin) = result {
            assert_eq!(Some(pin.workspace), registration.workspace);
            assert_eq!(Some(pin.agent), registration.agent);
            assert_eq!(pin.source_revision, registration.source_revision);
            assert_eq!(Some(pin.fingerprint), snapshot.envelope_fingerprint);
        }
    }

    #[kani::proof]
    fn every_duplicated_snapshot_pin_axis_is_binding() {
        let registration = SnapshotRegistrationIdentity {
            workspace: Some(1_u8),
            agent: Some(2_u8),
            source_revision: 3,
        };
        let mut snapshot = ExecutableSnapshotIdentity {
            root_agent: 2_u8,
            published_metadata: true,
            source_agent: 2_u8,
            source_revision: 3,
            envelope_fingerprint: Some(4_u8),
            resolved_fingerprint: Some(4_u8),
            metadata_fingerprint: Some(4_u8),
        };
        let axis: u8 = kani::any();
        kani::assume(axis < 5);

        match axis {
            0 => snapshot.root_agent = 5,
            1 => snapshot.source_agent = 5,
            2 => snapshot.source_revision = 5,
            3 => snapshot.resolved_fingerprint = Some(5),
            4 => snapshot.metadata_fingerprint = Some(5),
            _ => unreachable!(),
        }

        assert!(published_snapshot_pin(registration, snapshot).is_err());
    }
}
