//! Local Resources adapters for test and scenario Runtime Hosts.
//!
//! `awaken-session-executor` consumes Resources only through ports; it does not
//! link the Resources application or any store. A test or scenario
//! composition that wants a local Host builds its ports here, from the same
//! Resources authorities its process selected, and hands them to
//! `HostDependencies::for_local_resources`.

use std::path::Path;
use std::sync::Arc;

use awaken_ext_memory::MemoryExtractionRepository;
use awaken_resource_application::{
    ResourceAuthorities, StoreSkillBundleSource, StoreSkillCatalogApplication,
};
use awaken_resource_contract::SkillStore;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_session_executor::{
    DeploymentConfig, HostDependencies, LocalResourcePorts, LocalSkillPorts, SharedHost,
};
use awaken_time::WallClock;

/// The Host's Resource ports over one selected set of Resources
/// authorities: their Memory, File and Skill stores, reclamation state and
/// clock. Consumer: the CLI's local runtime process and the Scenario host.
#[must_use]
pub fn local_resource_ports(authorities: &ResourceAuthorities) -> LocalResourcePorts {
    LocalResourcePorts {
        memory: authorities.memory_execution_content(),
        file_store: authorities.file_store(),
        file_catalog: authorities.file_catalog(),
        skills: Some(local_skill_ports(
            authorities.skill_store(),
            authorities.clock(),
        )),
        reclamation: Some(authorities.reclamation()),
        clock: authorities.clock(),
    }
}

/// The Skill catalog ports over `store`: the store and the Resources
/// applications the Host reads and freezes custom Skills through.
#[must_use]
pub fn local_skill_ports(store: Arc<dyn SkillStore>, clock: Arc<dyn WallClock>) -> LocalSkillPorts {
    LocalSkillPorts {
        bundle_source: Arc::new(StoreSkillBundleSource::new(store.clone())),
        application: Arc::new(StoreSkillCatalogApplication::new(store.clone(), clock)),
        store,
    }
}

/// The local Memory extraction repository under one storage-dir durability
/// rule: `Some(dir)` opens `dir/sessions.db` (creating `dir`), `None` an
/// in-memory database that dies with the process.
///
/// # Panics
/// When the directory or the SQLite database cannot be opened; a fixture
/// that cannot open its storage has no meaningful Host to build.
#[must_use]
pub async fn memory_extraction_repository(
    storage_dir: Option<&Path>,
) -> Arc<dyn MemoryExtractionRepository> {
    match storage_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir)
                .expect("create durable Memory extraction repository directory");
            Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(
                    &dir.join("sessions.db").to_string_lossy(),
                )
                .await
                .expect("open durable Memory extraction repository"),
            )
        }
        None => Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .expect("open ephemeral Memory extraction repository"),
        ),
    }
}

/// A volatile Host over `llm`: [`volatile_dependencies`] turned into a Host.
/// Consumer: Worker and Run-ingress tests that need a bare Host.
///
/// # Panics
/// When an ephemeral database cannot be opened.
#[must_use]
pub async fn volatile_host(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
) -> Arc<SharedHost> {
    (volatile_dependencies(llm, model_ref).await)
        .build()
        .expect("validated Host dependencies")
}

/// Volatile Host dependencies over `llm`: ephemeral Memory and extraction
/// databases, in-memory Files, no Skill catalog or reclamation state, the
/// system clock, the reference volatile Runtime authority, and the Workdir
/// Session environment as a fallback Deployment selection replaces.
/// Consumer: tests that select further dependencies before building a Host.
///
/// # Panics
/// When an ephemeral database cannot be opened.
#[must_use]
pub async fn volatile_dependencies(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
) -> HostDependencies {
    volatile_dependencies_with_deployment(llm, model_ref, DeploymentConfig::ephemeral()).await
}

/// [`volatile_dependencies`] over an explicit typed Deployment. Consumer:
/// startup tests of the compositions that select a Session environment from it.
///
/// # Panics
/// When an ephemeral database cannot be opened.
#[must_use]
pub async fn volatile_dependencies_with_deployment(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    deployment: DeploymentConfig,
) -> HostDependencies {
    let resources = volatile_resource_ports().await;
    let authority = volatile_runtime_authority(resources.clock.clone());
    let base = awaken_session_executor::session_sandbox_base(&deployment);
    with_workdir_fallback(
        HostDependencies::for_local_resources(
            llm,
            model_ref,
            resources,
            memory_extraction_repository(None).await,
            SharedHost::mint_local_workspace(),
            deployment,
            authority,
        ),
        base,
    )
}

/// Select the Workdir adapter under the Deployment's sandbox root `base` as
/// the non-authoritative fallback Session provider.
fn with_workdir_fallback(
    dependencies: HostDependencies,
    base: std::path::PathBuf,
) -> HostDependencies {
    dependencies.with_fallback_session_environment_provider(
        awaken_session_environment::SandboxEnvironmentProvider::workdir(base)
            .into_session_provider(),
    )
}

/// Select the one reference Dispatch/commit authority at a test composition
/// root. Call only when no explicit runtime authority was already selected.
#[must_use]
pub fn volatile_runtime_authority(
    clock: Arc<dyn awaken_time::WallClock>,
) -> Arc<dyn awaken_session_executor::RuntimeAuthority> {
    // Cause/effect fixture rule: C1 a volatile scenario selects its authority
    // before Host construction; C2 the same clock stamps Dispatch and Thread commits.
    // R1 C1+C2 -> one Ingress MemoryDispatchStore is selected here, retains its
    // continuation and operational-feed roles through AnyCoordinatorDispatch, and is injected once;
    // product Host construction never selects a Coordinator backend implicitly.
    let dispatch = awaken_dispatch::AnyCoordinatorDispatch::from_durable_store(
        awaken_dispatch::MemoryDispatchStore::new(clock.clone()),
        None,
    );
    Arc::new(
        awaken_session_executor::EphemeralRuntimeAuthority::with_dispatch(clock, Arc::new(dispatch)),
    )
}

/// Upgrade `dependencies` to local durable storage under `dir`, with the
/// canonical extraction repository at `dir/sessions.db`. Consumer: tests whose
/// Host must survive a restart over the same directory.
///
/// # Panics
/// When `dir` or its extraction database cannot be opened.
#[must_use]
pub async fn with_durable_store_dir(
    dependencies: HostDependencies,
    dir: &Path,
) -> HostDependencies {
    dependencies.with_durable_store_dir(
        dir,
        memory_extraction_repository(Some(dir)).await,
        |provider, base| {
            awaken_session_environment::test_support::relocated_provider(provider, base)
        },
    )
}

async fn volatile_resource_ports() -> LocalResourcePorts {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let clock = awaken_time::system_clock();
    let memory = Arc::new(
        awaken_memory_store::SqliteMemoryRepository::open_in_memory(clock.clone())
            .await
            .expect("open ephemeral memory-fs sqlite store"),
    );
    LocalResourcePorts {
        memory: awaken_resource_contract::MemoryExecutionContent::new(memory),
        file_store: files.clone(),
        file_catalog: files,
        skills: None,
        reclamation: None,
        clock,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingMemoryInvalidator(std::sync::Mutex<Vec<(String, String)>>);
    #[cfg(test)]
    impl awaken_resource_contract::MemoryInvalidator for RecordingMemoryInvalidator {
        fn publish(&self, store: &str, path: &str) {
            self.0
                .lock()
                .expect("test Memory invalidations")
                .push((store.into(), path.into()));
        }
    }

    /// Decision table (storage dir → extraction repository):
    ///
    /// | Case | storage dir | Effect                                     |
    /// | E1   | none        | in-memory; nothing written to disk         |
    /// | E2   | some(dir)   | `dir` created, `dir/sessions.db` opened    |
    #[tokio::test]
    async fn extraction_repository_follows_the_storage_dir_rule() {
        let root = tempfile::tempdir().expect("tempdir");
        let _ = memory_extraction_repository(None).await;
        assert_eq!(
            std::fs::read_dir(root.path()).expect("read root").count(),
            0,
            "E1 writes nothing"
        );

        let dir = root.path().join("nested");
        let _ = memory_extraction_repository(Some(&dir)).await;
        assert!(
            dir.join("sessions.db").is_file(),
            "E2 opens dir/sessions.db"
        );
    }

    /// Decision table (authorities → Host ports): P1 every port the Host
    /// reads comes from the one selected set of authorities, so a Skill,
    /// reclamation state and clock are always present together; P2 the Host's
    /// clock is the exact Resources handle used to stamp and judge records.
    #[tokio::test]
    async fn resource_ports_carry_every_authority_of_one_selection() {
        let clock = awaken_time::system_clock();
        let resources = awaken_resource_persistence::ephemeral(
            clock,
            Arc::new(RecordingMemoryInvalidator::default()),
        )
        .await
        .expect("ephemeral Resources");
        let authorities = resources.authorities();
        let ports = local_resource_ports(&authorities);
        assert!(ports.skills.is_some(), "P1 Skill catalog present");
        assert!(ports.reclamation.is_some(), "P1 reclamation present");
        assert!(Arc::ptr_eq(&ports.clock, &authorities.clock()), "P2 clock");
        assert!(
            Arc::ptr_eq(&ports.memory.queries(), &authorities.memory_queries()),
            "P1 the same Memory authority"
        );
    }
}
