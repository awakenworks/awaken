//! Operator-authored limits for node image preparation; no Session authority.
//! The policy value lives in the provisioning contract so the deployment
//! configuration can carry it without linking this provider.

pub use awaken_container_contract::K8sImagePrewarm;
