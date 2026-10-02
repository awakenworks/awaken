//! Local Resources adapters for test and scenario Runtime Hosts.
//!
//! `awaken-session-executor` consumes Resources only through ports; it does not
//! link the Resources application or any store. A test or scenario
//! composition that wants a local Host builds its ports here, from the same
//! Resources authorities its process selected, and hands them to
//! `SharedHost::new_with_resources*`.

use std::path::Path;
use std::sync::Arc;

use awaken_ext_memory::MemoryExtractionRepository;
use awaken_lease_contract::WallClock;
use awaken_resource_application::{
    ResourceAuthorities, StoreSkillBundleSource, StoreSkillCatalogApplication,
};
use awaken_resource_contract::SkillStore;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_session_executor::{DeploymentConfig, LocalResourcePorts, LocalSkillPorts, SharedHost};

/// The Host's Resource ports over one selected set of Resources
/// authorities: their Memory, File and Skill stores, reclamation state and
/// clock. Consumer: the CLI's local runtime process and the Scenario host.
#[must_use]
pub fn local_resource_ports(authorities: &ResourceAuthorities) -> LocalResourcePorts {
    LocalResourcePorts {
        memory_repository: authorities.memory_repository(),
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
pub fn memory_extraction_repository(
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
                .expect("open durable Memory extraction repository"),
            )
        }
        None => Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("open ephemeral Memory extraction repository"),
        ),
    }
}

/// A volatile Host over `llm`: ephemeral Memory and extraction databases,
/// in-memory Files, no Skill catalog or reclamation state, the system clock,
/// and the Workdir Session environment as a fallback Deployment selection
/// replaces. Consumer: Worker and Run-ingress tests that need a bare Host.
///
/// # Panics
/// When an ephemeral database cannot be opened.
#[must_use]
pub fn volatile_host(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> SharedHost {
    with_workdir_fallback(SharedHost::new_with_resources(
        llm,
        model_ref,
        volatile_resource_ports(),
        memory_extraction_repository(None),
    ))
}

/// [`volatile_host`] over an explicit typed Deployment. Consumer: startup
/// tests of the compositions that select a Session environment from it.
///
/// # Panics
/// When an ephemeral database cannot be opened.
#[must_use]
pub fn volatile_host_with_deployment(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    deployment: DeploymentConfig,
) -> SharedHost {
    with_workdir_fallback(SharedHost::new_with_resources_and_deployment(
        llm,
        model_ref,
        volatile_resource_ports(),
        memory_extraction_repository(None),
        SharedHost::mint_local_workspace(),
        deployment,
    ))
}

/// Install the Workdir adapter under the Deployment's sandbox root as the
/// non-authoritative fallback Session provider.
fn with_workdir_fallback(host: SharedHost) -> SharedHost {
    let host = with_volatile_runtime_authority(host);
    let base = awaken_session_executor::session_sandbox_base(host.deployment_config());
    host.with_fallback_session_environment_provider(
        awaken_session_environment::SandboxEnvironmentProvider::workdir(base)
            .into_session_provider(),
    )
}

/// Install the one reference Dispatch/commit authority at a test composition
/// root. Call only when no explicit runtime authority was already selected.
#[must_use]
pub fn with_volatile_runtime_authority(host: SharedHost) -> SharedHost {
    // Cause/effect fixture rule: C1 a volatile scenario Host has no injected
    // authority; C2 the same Host clock stamps Dispatch and Thread commits.
    // R1 C1+C2 -> one Ingress MemoryDispatchStore is selected here, retains its
    // durable continuation role through AnyDispatchStore, and is injected once;
    // product Host construction never selects a Coordinator backend implicitly.
    let clock = host.clock();
    let dispatch = awaken_dispatch::AnyDispatchStore::from_durable_dispatch(Arc::new(
        awaken_dispatch::MemoryDispatchStore::new(clock.clone()),
    ));
    host.with_runtime_authority(Arc::new(
        awaken_session_executor::EphemeralRuntimeAuthority::with_dispatch(clock, Arc::new(dispatch)),
    ))
}

/// Upgrade `host` to local durable storage under `dir`, with the canonical
/// extraction repository at `dir/sessions.db`. Consumer: tests whose Host
/// must survive a restart over the same directory.
///
/// # Panics
/// When `dir` or its extraction database cannot be opened.
#[must_use]
pub fn with_durable_store_dir(host: SharedHost, dir: &Path) -> SharedHost {
    host.with_durable_store_dir(
        dir,
        memory_extraction_repository(Some(dir)),
        |provider, base| {
            awaken_session_environment::test_support::relocated_provider(provider, base)
        },
    )
}

fn volatile_resource_ports() -> LocalResourcePorts {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let clock = awaken_lease_contract::system_clock();
    LocalResourcePorts {
        memory_repository: Arc::new(
            awaken_memory_store::SqliteMemoryRepository::open_in_memory(clock.clone())
                .expect("open ephemeral memory-fs sqlite store"),
        ),
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

    /// Decision table (storage dir → extraction repository):
    ///
    /// | Case | storage dir | Effect                                     |
    /// | E1   | none        | in-memory; nothing written to disk         |
    /// | E2   | some(dir)   | `dir` created, `dir/sessions.db` opened    |
    #[test]
    fn extraction_repository_follows_the_storage_dir_rule() {
        let root = tempfile::tempdir().expect("tempdir");
        let _ = memory_extraction_repository(None);
        assert_eq!(
            std::fs::read_dir(root.path()).expect("read root").count(),
            0,
            "E1 writes nothing"
        );

        let dir = root.path().join("nested");
        let _ = memory_extraction_repository(Some(&dir));
        assert!(
            dir.join("sessions.db").is_file(),
            "E2 opens dir/sessions.db"
        );
    }

    /// Decision table (authorities → Host ports): P1 every port the Host
    /// reads comes from the one selected set of authorities, so a Skill,
    /// reclamation state and clock are always present together; P2 the Host's
    /// clock is the exact Resources handle used to stamp and judge records.
    #[test]
    fn resource_ports_carry_every_authority_of_one_selection() {
        let clock = awaken_lease_contract::system_clock();
        let resources = awaken_resource_persistence::ephemeral(clock).expect("ephemeral Resources");
        let authorities = resources.authorities();
        let ports = local_resource_ports(&authorities);
        assert!(ports.skills.is_some(), "P1 Skill catalog present");
        assert!(ports.reclamation.is_some(), "P1 reclamation present");
        assert!(Arc::ptr_eq(&ports.clock, &authorities.clock()), "P2 clock");
        assert!(
            Arc::ptr_eq(&ports.memory_repository, &authorities.memory_repository()),
            "P1 the same Memory authority"
        );
    }
}
