//! What one Worker incarnation may do now, and what admits it to the Registry.
//!
//! [`WorkerSnapshot::standing`] is the single decision over the Registry
//! lifecycle state and lease expiry. Callers match on it and add only their own
//! context explicitly (protocol support, capability fingerprint, a lease bound).
//! The registration and heartbeat admissions below are shared by the Registry
//! transitions, which enforce them inside the slot transaction, and by the
//! Coordinator application, which applies them before any provider effect.

use crate::{
    RegisteredWorker, RegistryError, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
    WorkerSnapshot, WorkerState,
};

/// The standing of one Worker incarnation at a given instant.
///
/// `Dead` takes precedence over `Expired`, and `Expired` over every live state:
/// a lapsed Registry lease removes all authority whatever the recorded state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStanding {
    /// Live but not ready: keeps the authority it already holds, takes no new work.
    Starting,
    /// Live and ready: the only standing that may take new work.
    Ready,
    /// Live and finishing admitted work; takes no new work.
    Draining,
    /// Live and drained with nothing in flight; its slot may be replaced.
    Quiesced,
    /// The Registry lease lapsed; the incarnation holds no authority.
    Expired,
    /// Deregistered or expired by the Registry.
    Dead,
}

impl WorkerStanding {
    /// Whether the incarnation may still exercise authority it already holds
    /// (renew, commit, Session realization, Resource access) and start the
    /// physical attempt of a claim it holds. Quiesced has published that it
    /// holds nothing; Expired and Dead hold no authority at all.
    #[must_use]
    pub const fn holds_authority(self) -> bool {
        matches!(self, Self::Starting | Self::Ready | Self::Draining)
    }
}

impl WorkerSnapshot {
    /// The one decision of what this incarnation may do at `now_ms`.
    #[must_use]
    pub const fn standing(&self, now_ms: u64) -> WorkerStanding {
        match self.state {
            WorkerState::Dead => WorkerStanding::Dead,
            _ if self.expires_at_ms <= now_ms => WorkerStanding::Expired,
            WorkerState::Starting => WorkerStanding::Starting,
            WorkerState::Ready => WorkerStanding::Ready,
            WorkerState::Draining => WorkerStanding::Draining,
            WorkerState::Quiesced => WorkerStanding::Quiesced,
        }
    }

    /// Whether `heartbeat` changes evidence that placement reads. This is a
    /// reconciliation hint only; the Registry heartbeat remains the authority.
    #[must_use]
    pub fn placement_changed_by(&self, heartbeat: &WorkerHeartbeat) -> bool {
        self.state.accepts_work() != heartbeat.ready
            || self.in_flight != heartbeat.in_flight
            || self.warm_environment_shapes != heartbeat.warm_environment_shapes
            || self.credential_observations != heartbeat.credential_observations
            || self.acp_capability_observations != heartbeat.acp_capability_observations
    }
}

/// The credential a registering process authenticated with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationCredential {
    /// The pre-registration bootstrap credential: the only one that registers.
    Bootstrap,
    /// A credential already bound to an allocated incarnation.
    Incarnation,
}

/// The protocol floor of every Registry admission: an immutable manifest
/// without explicit current-protocol support is never admitted or renewed.
pub fn require_current_runtime_protocol(manifest: &WorkerManifest) -> Result<(), RegistryError> {
    if manifest.supports_current_runtime_protocol() {
        Ok(())
    } else {
        Err(RegistryError::UnsupportedRuntimeProtocol)
    }
}

/// Admission of a registration request before any provider effect.
pub fn worker_registration_request_admission(
    credential: RegistrationCredential,
    manifest: &WorkerManifest,
) -> Result<(), RegistryError> {
    if credential != RegistrationCredential::Bootstrap {
        return Err(RegistryError::BootstrapCredentialRequired);
    }
    require_current_runtime_protocol(manifest)
}

/// The incarnation a registration is admitted under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationIncarnation {
    /// No provider identifies this process; its proposed incarnation stands.
    Proposed,
    /// The provider observed this incarnation (a Kubernetes container
    /// instance); it replaces whatever the process proposed.
    ProviderObserved(String),
}

/// Decide the registering incarnation from the provider observation.
///
/// A process in a provider-identified domain is admitted only under the
/// incarnation the provider observes; its own proposal is never trusted.
pub fn registration_incarnation_admission(
    observed: Option<String>,
    provider_identified: bool,
) -> Result<RegistrationIncarnation, RegistryError> {
    match observed {
        Some(incarnation) => Ok(RegistrationIncarnation::ProviderObserved(incarnation)),
        None if provider_identified => Err(RegistryError::ProviderIncarnationRequired),
        None => Ok(RegistrationIncarnation::Proposed),
    }
}

/// The immutable manifest cannot gain protocol support by heartbeat. Only the
/// exact current incarnation is judged; a stale identity keeps the Registry's
/// ordinary non-mutating rejection, and drain/quiesce/deregister stay open.
pub fn worker_heartbeat_protocol_admission(
    current: Option<&RegisteredWorker>,
    identity: &WorkerIdentity,
) -> Result<(), RegistryError> {
    match current {
        Some(record) if &record.snapshot.identity == identity => {
            require_current_runtime_protocol(&record.snapshot.manifest)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CURRENT_RUNTIME_PROTOCOL_VERSION, VersionRange};

    fn manifest(supported: bool) -> WorkerManifest {
        let mut manifest = WorkerManifest::default();
        if supported {
            manifest.runtime_protocol = VersionRange::exact(CURRENT_RUNTIME_PROTOCOL_VERSION);
        }
        manifest
    }

    fn snapshot(state: WorkerState, expires_at_ms: u64) -> WorkerSnapshot {
        let manifest = manifest(true);
        WorkerSnapshot {
            identity: WorkerIdentity::new("worker", "boot", 1),
            state,
            capability_fingerprint: manifest.fingerprint().unwrap(),
            manifest,
            in_flight: 0,
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            expires_at_ms,
        }
    }

    #[test]
    fn standing_decision_table() {
        // Causes: C1 recorded state; C2 lease live (expires_at > now).
        // Dead wins over expiry; expiry wins over every live state; a live
        // record keeps its state. The boundary instant is already expired.
        use WorkerStanding as S;
        let rows = [
            (WorkerState::Starting, true, S::Starting),
            (WorkerState::Ready, true, S::Ready),
            (WorkerState::Draining, true, S::Draining),
            (WorkerState::Quiesced, true, S::Quiesced),
            (WorkerState::Dead, true, S::Dead),
            (WorkerState::Starting, false, S::Expired),
            (WorkerState::Ready, false, S::Expired),
            (WorkerState::Draining, false, S::Expired),
            (WorkerState::Quiesced, false, S::Expired),
            (WorkerState::Dead, false, S::Dead),
        ];
        for (state, live, expected) in rows {
            let now = 100;
            let expires_at = if live { now + 1 } else { now };
            assert_eq!(
                snapshot(state, expires_at).standing(now),
                expected,
                "{state:?} live={live}"
            );
        }
    }

    #[test]
    fn held_authority_decision_table() {
        // Only a live incarnation that has not published quiescence keeps the
        // authority it holds; a drained, lapsed or dead one keeps none.
        use WorkerStanding as S;
        for (standing, holds) in [
            (S::Starting, true),
            (S::Ready, true),
            (S::Draining, true),
            (S::Quiesced, false),
            (S::Expired, false),
            (S::Dead, false),
        ] {
            assert_eq!(standing.holds_authority(), holds, "{standing:?}");
        }
    }

    #[test]
    fn registration_request_admission_decision_table() {
        // R1 bootstrap + current protocol => admitted. R2 an incarnation
        // credential never registers, whatever the manifest. R3 bootstrap
        // without explicit current protocol => refused.
        use RegistrationCredential as C;
        let rows: [(C, bool, Option<&str>); 4] = [
            (C::Bootstrap, true, None),
            (C::Incarnation, true, Some("bootstrap")),
            (C::Incarnation, false, Some("bootstrap")),
            (C::Bootstrap, false, Some("current runtime protocol")),
        ];
        for (credential, supported, refusal) in rows {
            let outcome = worker_registration_request_admission(credential, &manifest(supported));
            match refusal {
                None => assert!(outcome.is_ok(), "{credential:?}/{supported}"),
                Some(text) => assert!(
                    outcome.unwrap_err().to_string().contains(text),
                    "{credential:?}/{supported}"
                ),
            }
        }
        for (min, max) in [(0, u32::MAX), (0, 0), (1, 1)] {
            let legacy = WorkerManifest {
                runtime_protocol: VersionRange { min, max },
                ..WorkerManifest::default()
            };
            assert!(
                matches!(
                    require_current_runtime_protocol(&legacy),
                    Err(RegistryError::UnsupportedRuntimeProtocol)
                ),
                "an omitted or older range carries no positive evidence: {min}..{max}"
            );
        }
    }

    #[test]
    fn registration_incarnation_decision_table() {
        // I1 provider observation replaces the proposal, identified or not.
        // I2 a provider-identified domain without an observation is refused:
        // the self-proposed incarnation is never trusted. I3 an unidentified
        // domain without an observation keeps its proposal.
        let rows = [
            (
                Some("c-1"),
                true,
                Some(RegistrationIncarnation::ProviderObserved("c-1".into())),
            ),
            (
                Some("c-1"),
                false,
                Some(RegistrationIncarnation::ProviderObserved("c-1".into())),
            ),
            (None, true, None),
            (None, false, Some(RegistrationIncarnation::Proposed)),
        ];
        for (observed, identified, expected) in rows {
            let outcome = registration_incarnation_admission(observed.map(Into::into), identified);
            match expected {
                Some(expected) => assert_eq!(outcome.unwrap(), expected),
                None => assert!(matches!(
                    outcome,
                    Err(RegistryError::ProviderIncarnationRequired)
                )),
            }
        }
    }

    #[test]
    fn heartbeat_protocol_admission_decision_table() {
        // H1 exact current + unsupported => refused. H2 exact current +
        // supported => admitted. H3 stale identity or absent record => left
        // to the ordinary non-mutating Registry outcome.
        let record = |supported| {
            let mut snapshot = snapshot(WorkerState::Ready, 10);
            snapshot.manifest = manifest(supported);
            RegisteredWorker {
                snapshot,
                heartbeat_sequence: 0,
                observation_sequence: 0,
                registered_at_ms: 0,
                heartbeat_at_ms: 0,
                drain_deadline_ms: None,
                process_exit: None,
                create_ledger: None,
                source_retention: Default::default(),
            }
        };
        let exact = WorkerIdentity::new("worker", "boot", 1);
        let stale = WorkerIdentity::new("worker", "boot", 2);
        assert!(matches!(
            worker_heartbeat_protocol_admission(Some(&record(false)), &exact),
            Err(RegistryError::UnsupportedRuntimeProtocol)
        ));
        assert!(worker_heartbeat_protocol_admission(Some(&record(true)), &exact).is_ok());
        assert!(worker_heartbeat_protocol_admission(Some(&record(false)), &stale).is_ok());
        assert!(worker_heartbeat_protocol_admission(None, &exact).is_ok());
    }

    #[test]
    fn placement_change_reads_each_placement_input() {
        let base = snapshot(WorkerState::Ready, 10);
        let heartbeat = WorkerHeartbeat {
            sequence: 1,
            ready: true,
            in_flight: 0,
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            source_retention: Default::default(),
        };
        assert!(!base.placement_changed_by(&heartbeat), "unchanged");
        let mut not_ready = heartbeat.clone();
        not_ready.ready = false;
        assert!(base.placement_changed_by(&not_ready), "readiness");
        let mut busy = heartbeat.clone();
        busy.in_flight = 1;
        assert!(base.placement_changed_by(&busy), "in flight");
        let mut warm = heartbeat.clone();
        warm.warm_environment_shapes = ["shape".into()].into();
        assert!(base.placement_changed_by(&warm), "warm capacity");
        let mut retention = heartbeat;
        retention.sequence = 9;
        retention.source_retention = Default::default();
        assert!(
            !base.placement_changed_by(&retention),
            "sequence and source retention are not placement inputs"
        );
    }
}
