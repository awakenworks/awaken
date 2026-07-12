//! Seam test: the `SandboxProvider` port is implementable outside the local impl
//! (a distributed provider plugs in from another repo), and `SandboxSpec` carries
//! `mounts` / `constraints` as forward-compatible data the local provider ignores
//! but a distributed one consumes. This fixes the single-machine <-> distributed
//! boundary: this repo ships only the local side, but the seam is stable.

use awaken_sandbox_local::{Environment, SandboxError, SandboxProvider, SandboxSpec};

/// A stand-in for a provider implemented in a distributed repository: it would use
/// `spec.mounts` / `spec.constraints` to provision a container or remote root. Here
/// it only proves the trait is implementable elsewhere and the data is carried.
struct DistributedStyleProvider;

#[async_trait::async_trait]
impl SandboxProvider for DistributedStyleProvider {
    async fn create(&self, spec: &SandboxSpec) -> Result<Environment, SandboxError> {
        // A real distributed provider reads the reserved fields; assert they arrive.
        let mount_count = spec.mounts.len();
        let has_constraints = spec.constraints.is_some();
        assert!(
            mount_count > 0 && has_constraints,
            "reserved seam data must be carried"
        );
        // A real distributed provider would bind relay tools here (from a
        // container/remote root); the seam only needs to prove the port is
        // implementable elsewhere and the reserved data arrives.
        Ok(Environment::new(spec.id.clone(), Vec::new()))
    }

    async fn teardown(&self, _id: &str) -> Result<(), SandboxError> {
        Ok(())
    }
}

#[tokio::test]
async fn provider_seam_accepts_a_distributed_impl_with_reserved_data() {
    let mut spec = SandboxSpec::new("env-x");
    spec.mounts = vec![serde_json::json!({ "type": "project", "mount_path": "/workspace" })];
    spec.constraints = Some(serde_json::json!({ "networking": "limited" }));

    let provider = DistributedStyleProvider;
    let env = provider.create(&spec).await.unwrap();

    assert_eq!(env.id(), "env-x");
    assert!(env.hand_tools().is_empty());
    provider.teardown("env-x").await.unwrap();
}
