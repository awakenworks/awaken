//! Deterministic Dream scenario composition shared by Rust and TypeScript E2E.
//!
//! Production requires a real write-through FUSE mount. The conformance scenario
//! replaces only that infrastructure port so orchestration can run on CI hosts
//! without `/dev/fuse`; write-through enforcement itself remains covered by the
//! provisioning decision-table tests.

use std::path::Path;
use std::sync::Arc;

use awaken_memory_store::{MemErr, MemoryRepository};
use awaken_runtime_host::{MemoryMount, MemoryMounter, MountAccess, Realization, SandboxError};
use axum::Router;

use crate::{EchoModel, SharedHost, build_router_and_host};

struct ScenarioDreamMemoryMounter {
    memory: Arc<dyn MemoryRepository>,
}

struct ScenarioDreamMemoryMount;

#[async_trait::async_trait]
impl MemoryMount for ScenarioDreamMemoryMount {
    fn realization(&self) -> Realization {
        Realization::Fuse
    }

    async fn teardown(self: Box<Self>) {}
}

fn sandbox_error(error: impl std::fmt::Display) -> SandboxError {
    SandboxError::new(error.to_string())
}

#[async_trait::async_trait]
impl MemoryMounter for ScenarioDreamMemoryMounter {
    async fn mount(
        &self,
        store_id: &str,
        host_path: &Path,
        _access: MountAccess,
    ) -> Result<Box<dyn MemoryMount>, SandboxError> {
        std::fs::create_dir_all(host_path).map_err(sandbox_error)?;
        let memories = self
            .memory
            .snapshot_heads(store_id)
            .await
            .map_err(|error: MemErr| sandbox_error(error))?;
        for memory in memories {
            let path = host_path.join(memory.path.trim_start_matches('/'));
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(sandbox_error)?;
            }
            std::fs::write(path, memory.content.unwrap_or_default()).map_err(sandbox_error)?;
        }
        Ok(Box::new(ScenarioDreamMemoryMount))
    }
}

/// Build the one Dream scenario router used by process-level SDK E2E.
pub fn build_dream_router() -> Router {
    build_dream_router_and_host().0
}

/// Return the same router plus its Host for focused cross-module assertions.
pub fn build_dream_router_and_host() -> (Router, Arc<SharedHost>) {
    let (router, host) = build_router_and_host(Arc::new(EchoModel), "claude-sonnet-5");
    host.install_memory_mounter(Arc::new(ScenarioDreamMemoryMounter {
        memory: host.memory_repository(),
    }));
    (router, host)
}
