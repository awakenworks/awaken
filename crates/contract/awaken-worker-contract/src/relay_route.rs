// @formal surface obligation=worker.incarnation_generation_fences_stale_processes
// @formal reason: Secret-free network destination projected only after the current Worker
// @formal reason: identity and Session realization lease are checked by their existing owners.
//! Exact Worker relay transport coordinate, never a credential or route grant.

pub const WORKER_RELAY_DESTINATION_PATH: &str = "/internal/v1/workers/mcp-relay-destination";

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::WorkerIdentity;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "WorkerRelayDestinationWire",
    into = "WorkerRelayDestinationWire"
)]
pub struct WorkerRelayDestination {
    worker: WorkerIdentity,
    ip: IpAddr,
    expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerRelayDestinationWire {
    worker: WorkerIdentity,
    ip: IpAddr,
    expires_at_unix_ms: u64,
}

impl WorkerRelayDestination {
    pub fn new(
        worker: WorkerIdentity,
        ip: IpAddr,
        expires_at_unix_ms: u64,
    ) -> Result<Self, &'static str> {
        if worker.worker_id.trim().is_empty()
            || worker.worker_id.trim() != worker.worker_id
            || worker.incarnation_id.trim().is_empty()
            || worker.incarnation_id.trim() != worker.incarnation_id
            || worker.generation == 0
            || invalid_relay_ip(ip)
            || expires_at_unix_ms == 0
        {
            return Err("invalid exact Worker relay destination");
        }
        Ok(Self {
            worker,
            ip,
            expires_at_unix_ms,
        })
    }

    #[must_use]
    pub fn worker(&self) -> &WorkerIdentity {
        &self.worker
    }

    #[must_use]
    pub const fn ip(&self) -> IpAddr {
        self.ip
    }

    #[must_use]
    pub const fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }
}

fn invalid_relay_ip(ip: IpAddr) -> bool {
    let address = match ip {
        IpAddr::V4(address) => address,
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return invalid_relay_ip(IpAddr::V4(mapped));
            }
            return address.is_loopback() || address.is_unspecified() || address.is_multicast();
        }
    };
    address.is_loopback() || address.is_unspecified() || address.is_multicast()
}

impl TryFrom<WorkerRelayDestinationWire> for WorkerRelayDestination {
    type Error = &'static str;

    fn try_from(wire: WorkerRelayDestinationWire) -> Result<Self, Self::Error> {
        Self::new(wire.worker, wire.ip, wire.expires_at_unix_ms)
    }
}

impl From<WorkerRelayDestination> for WorkerRelayDestinationWire {
    fn from(destination: WorkerRelayDestination) -> Self {
        Self {
            worker: destination.worker,
            ip: destination.ip,
            expires_at_unix_ms: destination.expires_at_unix_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_destination_roundtrips_and_rejects_forged_coordinates() {
        // Causes: C1 exact nonzero Worker incarnation; C2 a non-loopback Pod
        // address; C3 nonzero expiry; C4 blank identity, local address or
        // unknown wire member. Effects: E1 one stable secret-free JSON value;
        // E2 invalid or widened bytes fail at construction and deserialization.
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // | R1 | yes | yes | yes | no | E1 |
        // | R2 | no | any | any | yes | E2 |
        // | R3 | yes | no | any | yes | E2 |
        // | R4 | yes | yes | no | any | E2 |
        let worker = WorkerIdentity::new("k8s:ns:worker:uid", "uid", 3);
        let ip: IpAddr = "10.1.2.3".parse().unwrap();
        let target = WorkerRelayDestination::new(worker.clone(), ip, 100).unwrap();
        let encoded = serde_json::to_value(target.clone()).unwrap();
        assert_eq!(
            serde_json::from_value::<WorkerRelayDestination>(encoded.clone()).unwrap(),
            target,
            "R1/E1"
        );
        assert_eq!(target.worker(), &worker, "R1/E1");
        assert_eq!(target.ip(), ip, "R1/E1");
        assert_eq!(target.expires_at_unix_ms(), 100, "R1/E1");

        assert!(
            WorkerRelayDestination::new(worker.clone(), ip, 0).is_err(),
            "R4/E2"
        );
        assert!(
            WorkerRelayDestination::new(WorkerIdentity::new("", "uid", 3), ip, 100).is_err(),
            "R2/E2"
        );
        assert!(
            WorkerRelayDestination::new(worker, IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 100)
                .is_err(),
            "R3/E2"
        );
        assert!(
            WorkerRelayDestination::new(
                WorkerIdentity::new("k8s:ns:worker:uid", "uid", 3),
                "::ffff:127.0.0.1".parse().unwrap(),
                100,
            )
            .is_err(),
            "R3/E2 mapped loopback"
        );
        let mut widened = encoded;
        widened["unexpected"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<WorkerRelayDestination>(widened).is_err(),
            "R3/E2"
        );
    }
}
