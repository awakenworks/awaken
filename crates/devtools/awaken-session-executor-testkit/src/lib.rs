//! Local Resources adapters for test and scenario Runtime Hosts.
//!
//! `awaken-session-executor` consumes Resources only through ports; it does not
//! link the Resources application or any store. A test or scenario
//! composition that wants a local Host builds its ports here, from the same
//! Resources authorities its process selected, and hands them to
//! `SessionExecutorBuilder::for_local_resources`.

use std::path::Path;
use std::sync::Arc;

use awaken_ext_memory::MemoryExtractionRepository;
use awaken_resource_application::{
    ResourceAuthorities, StoreSkillBundleSource, StoreSkillCatalogApplication,
};
use awaken_resource_contract::SkillStore;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_session_executor::{
    DeploymentConfig, ExecutorComponents, LocalResourcePorts, LocalSkillPorts,
    SessionExecutorBuilder,
};
use awaken_time::WallClock;

/// The Host's Resource ports over one selected set of Resources
/// authorities: their Memory, File and Skill stores and reclamation state.
/// The same root gives the Resources clock to its SessionExecutionProcess.
/// Consumer: the CLI's local runtime process and the Scenario host.
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
) -> Arc<ExecutorComponents> {
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
) -> SessionExecutorBuilder {
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
) -> SessionExecutorBuilder {
    // Preserve the original fixture opening order while the process owns its
    // sole clock: Files, clock, Memory SQLite, authority, sandbox base,
    // extraction SQLite, Workspace, model conversion, then process projection.
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let clock = awaken_time::system_clock();
    let memory = Arc::new(
        awaken_memory_store::SqliteMemoryRepository::open_in_memory(clock.clone())
            .await
            .expect("open ephemeral memory-fs sqlite store"),
    );
    let resources = LocalResourcePorts {
        memory: awaken_resource_contract::MemoryExecutionContent::new(memory),
        file_store: files.clone(),
        file_catalog: files,
        skills: None,
        reclamation: None,
    };
    let authority = volatile_runtime_authority(clock.clone());
    let base = awaken_session_executor::session_sandbox_base(&deployment);
    let extraction_repository = memory_extraction_repository(None).await;
    let local_workspace = ExecutorComponents::mint_local_workspace();
    let model_ref: String = model_ref.into();
    let process = deployment
        .session_execution_process(local_workspace, clock)
        .expect("a local fixture selects a valid process");
    with_workdir_fallback(
        SessionExecutorBuilder::for_local_resources(
            llm,
            model_ref,
            resources,
            extraction_repository,
            process,
            authority,
        ),
        base,
    )
}

/// Select the Workdir adapter under the Deployment's sandbox root `base` as
/// the non-authoritative fallback Session provider.
fn with_workdir_fallback(
    dependencies: SessionExecutorBuilder,
    base: std::path::PathBuf,
) -> SessionExecutorBuilder {
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
) -> Arc<dyn awaken_dispatch_erasure::RuntimeAuthority> {
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
        awaken_session_executor::EphemeralRuntimeAuthority::with_dispatch(
            clock,
            Arc::new(dispatch),
        ),
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
    dependencies: SessionExecutorBuilder,
    dir: &Path,
) -> SessionExecutorBuilder {
    dependencies.with_durable_store_dir(
        dir,
        memory_extraction_repository(Some(dir)).await,
        |provider, base| {
            awaken_session_environment::test_support::relocated_provider(provider, base)
        },
    )
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

    /// Ownership-preservation graph: C1 no storage directory, C2 a selected
    /// directory; E1 opens only volatile SQLite, E2 creates that directory and
    /// opens its sessions.db. The Executor unit constructors now consume this
    /// same selection body through the common MemoryExtractionRepository port;
    /// no Executor value crosses the dev-only fixture edge. The original
    /// mkdir/open panic text and await position remain in this one body.
    /// These E1/E2 rows observe storage effects, not a live process restart or
    /// the failure branches; durable Host/Store suites retain those distinct
    /// responsibilities. The pure source move changes no storage decision.
    ///
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

    /// Cause/effect graph: C1 one Resources authority selection supplies the
    /// File/Memory/Skill/reclamation ports; C2 its clock fills the existing
    /// process value; C3 the sole local constructor consumes those inputs.
    /// P1 C1 -> the original Skill/reclamation/Memory identities are present.
    /// P2 C1+C2+C3 -> the actual Builder clock is that same Resources Arc,
    /// rather than a second clock copied into LocalResourcePorts. This row
    /// observes the constructed Builder; comparing two inputs is insufficient.
    /// Existing topology/kernel and root suites own invalid-deployment panic,
    /// provider/capture and authority rules; this row performs no model I/O and
    /// adds no parallel model fake, clock selection policy or permission path.
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
        assert!(
            Arc::ptr_eq(&ports.memory.queries(), &authorities.memory_queries()),
            "P1 the same Memory authority"
        );
        let process = DeploymentConfig::ephemeral()
            .session_execution_process(
                ExecutorComponents::mint_local_workspace(),
                authorities.clock(),
            )
            .expect("a local fixture selects a valid process");
        let dependencies = SessionExecutorBuilder::for_local_resources(
            Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
            awaken_session_executor::UNCONFIGURED_MODEL_REF,
            ports,
            memory_extraction_repository(None).await,
            process,
            volatile_runtime_authority(authorities.clock()),
        );
        assert!(
            Arc::ptr_eq(&dependencies.clock(), &authorities.clock()),
            "P2 clock"
        );
    }
}
