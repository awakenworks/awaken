use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerHeartbeat, WorkerIdentity,
    WorkerRegistration, WorkerSnapshot, WorkerState,
};

fn identity_matches(record: &RegisteredWorker, identity: &WorkerIdentity) -> bool {
    &record.snapshot.identity == identity
}

pub(crate) fn register(
    current: Option<&RegisteredWorker>,
    registration: WorkerRegistration,
    now_ms: u64,
    ttl_ms: u64,
) -> Result<(RegisteredWorker, bool), RegistryError> {
    if registration.worker_id.trim().is_empty() || registration.incarnation_id.trim().is_empty() {
        return Err(RegistryError::InvalidIdentity);
    }
    let fingerprint = registration.manifest.fingerprint().map_err(|error| {
        RegistryError::Persistence(format!("manifest fingerprint failed: {error}"))
    })?;
    if let Some(current) = current {
        if current.snapshot.identity.incarnation_id == registration.incarnation_id {
            if current.capability_fingerprint() != fingerprint {
                return Err(RegistryError::ManifestChanged);
            }
            return Ok((current.clone(), false));
        }
        let replaceable = matches!(
            current.snapshot.state,
            WorkerState::Quiesced | WorkerState::Dead
        ) || current.snapshot.expires_at_ms <= now_ms;
        if !replaceable {
            return Err(RegistryError::SlotOccupied {
                worker_id: registration.worker_id,
                generation: current.snapshot.identity.generation,
            });
        }
    }
    let generation = current.map_or(1, |record| {
        record.snapshot.identity.generation.saturating_add(1)
    });
    Ok((
        RegisteredWorker {
            snapshot: WorkerSnapshot {
                identity: WorkerIdentity::new(
                    registration.worker_id,
                    registration.incarnation_id,
                    generation,
                ),
                state: WorkerState::Starting,
                manifest: registration.manifest,
                capability_fingerprint: fingerprint,
                in_flight: 0,
                expires_at_ms: now_ms.saturating_add(ttl_ms),
            },
            heartbeat_sequence: 0,
            registered_at_ms: now_ms,
            heartbeat_at_ms: now_ms,
            drain_deadline_ms: None,
        },
        true,
    ))
}

trait Fingerprint {
    fn capability_fingerprint(&self) -> &str;
}

impl Fingerprint for RegisteredWorker {
    fn capability_fingerprint(&self) -> &str {
        &self.snapshot.capability_fingerprint
    }
}

pub(crate) fn heartbeat(
    current: Option<&RegisteredWorker>,
    identity: &WorkerIdentity,
    heartbeat: WorkerHeartbeat,
    now_ms: u64,
    ttl_ms: u64,
) -> (Option<RegisteredWorker>, RegistryMutation) {
    let Some(current) = current else {
        return (None, RegistryMutation::NotFound);
    };
    if !identity_matches(current, identity) {
        return (None, RegistryMutation::StaleIncarnation);
    }
    if heartbeat.sequence <= current.heartbeat_sequence {
        return (None, RegistryMutation::StaleSequence);
    }
    if matches!(
        current.snapshot.state,
        WorkerState::Quiesced | WorkerState::Dead
    ) {
        return (None, RegistryMutation::InvalidTransition);
    }
    let mut next = current.clone();
    if !matches!(next.snapshot.state, WorkerState::Draining) {
        next.snapshot.state = if heartbeat.ready {
            WorkerState::Ready
        } else {
            WorkerState::Starting
        };
    }
    next.snapshot.in_flight = heartbeat.in_flight;
    next.snapshot.expires_at_ms = now_ms.saturating_add(ttl_ms);
    next.heartbeat_sequence = heartbeat.sequence;
    next.heartbeat_at_ms = now_ms;
    (Some(next), RegistryMutation::Applied)
}

pub(crate) fn begin_drain(
    current: Option<&RegisteredWorker>,
    identity: &WorkerIdentity,
    deadline_ms: u64,
) -> (Option<RegisteredWorker>, RegistryMutation) {
    let Some(current) = current else {
        return (None, RegistryMutation::NotFound);
    };
    if !identity_matches(current, identity) {
        return (None, RegistryMutation::StaleIncarnation);
    }
    if matches!(
        current.snapshot.state,
        WorkerState::Quiesced | WorkerState::Dead
    ) {
        return (None, RegistryMutation::InvalidTransition);
    }
    let mut next = current.clone();
    next.snapshot.state = WorkerState::Draining;
    next.drain_deadline_ms = Some(deadline_ms);
    (Some(next), RegistryMutation::Applied)
}

pub(crate) fn quiesce(
    current: Option<&RegisteredWorker>,
    identity: &WorkerIdentity,
) -> (Option<RegisteredWorker>, RegistryMutation) {
    let Some(current) = current else {
        return (None, RegistryMutation::NotFound);
    };
    if !identity_matches(current, identity) {
        return (None, RegistryMutation::StaleIncarnation);
    }
    if current.snapshot.state != WorkerState::Draining || current.snapshot.in_flight != 0 {
        return (None, RegistryMutation::InvalidTransition);
    }
    let mut next = current.clone();
    next.snapshot.state = WorkerState::Quiesced;
    (Some(next), RegistryMutation::Applied)
}

pub(crate) fn deregister(
    current: Option<&RegisteredWorker>,
    identity: &WorkerIdentity,
) -> (Option<RegisteredWorker>, RegistryMutation) {
    let Some(current) = current else {
        return (None, RegistryMutation::NotFound);
    };
    if !identity_matches(current, identity) {
        return (None, RegistryMutation::StaleIncarnation);
    }
    let mut next = current.clone();
    next.snapshot.state = WorkerState::Dead;
    (Some(next), RegistryMutation::Applied)
}

pub(crate) fn expire(current: &RegisteredWorker, now_ms: u64) -> Option<RegisteredWorker> {
    if current.snapshot.expires_at_ms > now_ms
        || matches!(
            current.snapshot.state,
            WorkerState::Quiesced | WorkerState::Dead
        )
    {
        return None;
    }
    let mut next = current.clone();
    next.snapshot.state = WorkerState::Dead;
    Some(next)
}

pub(crate) const fn state_name(state: WorkerState) -> &'static str {
    match state {
        WorkerState::Starting => "starting",
        WorkerState::Ready => "ready",
        WorkerState::Draining => "draining",
        WorkerState::Quiesced => "quiesced",
        WorkerState::Dead => "dead",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_worker_contract::WorkerManifest;

    fn registration(incarnation: &str) -> WorkerRegistration {
        WorkerRegistration {
            worker_id: "worker-a".to_string(),
            incarnation_id: incarnation.to_string(),
            manifest: WorkerManifest::default(),
        }
    }

    #[test]
    fn live_slot_cannot_be_replaced_but_expired_slot_can() {
        let (first, _) = register(None, registration("boot-1"), 10, 100).unwrap();
        assert!(matches!(
            register(Some(&first), registration("boot-2"), 20, 100),
            Err(RegistryError::SlotOccupied { .. })
        ));
        let (second, changed) = register(Some(&first), registration("boot-2"), 111, 100).unwrap();
        assert!(changed);
        assert_eq!(second.snapshot.identity.generation, 2);
    }

    #[test]
    fn drain_is_absorbing_for_heartbeats() {
        let (first, _) = register(None, registration("boot-1"), 10, 100).unwrap();
        let identity = first.snapshot.identity.clone();
        let (draining, _) = begin_drain(Some(&first), &identity, 80);
        let draining = draining.unwrap();
        let (updated, result) = heartbeat(
            Some(&draining),
            &identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 1,
            },
            20,
            100,
        );
        assert_eq!(result, RegistryMutation::Applied);
        assert_eq!(updated.unwrap().snapshot.state, WorkerState::Draining);
    }
}
