//! RES capability of [`ExecutorComponents`]: the Resources-context ports the Host
//! consumes (Memory content and extraction, File content) and the test-support
//! local File/Skill/reclamation authorities. Artifact publication belongs to
//! the realization executor.

use crate::host::*;

/// Worker-side Resource content ports that must be installed as one unit.
/// Keeping the pair named prevents a partially remote Host startup from
/// silently opening a local content authority for the missing half.
pub struct WorkerContentAdapters {
    pub file_content_source: Arc<dyn crate::FileContentSource<crate::ResourceOperationFence>>,
    pub memory: awaken_resource_contract::MemoryExecutionContent,
}

/// Local Resource authorities a test or scenario composition hands to the
/// Host as one unit: the ports its Resources application exposes. Consumer:
/// `SessionExecutorBuilder::for_local_resources`; `awaken-session-executor-testkit` builds it
/// from a Resources application, which this crate no longer links.
#[cfg(any(test, feature = "test-support"))]
pub struct LocalResourcePorts {
    pub memory: awaken_resource_contract::MemoryExecutionContent,
    pub file_store: Arc<dyn awaken_resource_contract::FileStore>,
    pub file_catalog: Arc<dyn awaken_resource_contract::FileCatalog>,
    /// The durable delivered-Skill catalog, when the composition has one.
    pub skills: Option<LocalSkillPorts>,
    /// Resource lifecycle/reference state, when the composition has one.
    pub reclamation: Option<Arc<dyn awaken_resource_contract::ResourceReclamationRepository>>,
}

/// The durable Skill catalog and the two Resources applications over it that
/// the Host reads and freezes custom Skills through. Consumer:
/// [`LocalResourcePorts`].
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone)]
pub struct LocalSkillPorts {
    pub store: Arc<dyn awaken_resource_contract::SkillStore>,
    pub bundle_source: Arc<
        dyn awaken_session_configuration_contract::SkillBundleSource<crate::ResourceOperationFence>,
    >,
    pub application: Arc<dyn awaken_session_configuration_contract::SkillCatalogApplication>,
}

/// The Resource inputs of one Host construction, selected together so
/// [`ResourceSelection::assemble`] sees every source at once. Consumer:
/// the `SessionExecutorBuilder` constructors.
pub(in crate::host) struct ResourceInputs {
    /// Local authorities of a test/scenario composition.
    #[cfg(any(test, feature = "test-support"))]
    pub(in crate::host) local: Option<LocalResourcePorts>,
    /// Remote content adapters of a Worker or Coordinator-local Host.
    pub(in crate::host) worker_content: Option<WorkerContentAdapters>,
    /// Explicit Memory extraction authority.
    pub(in crate::host) extraction_repository:
        Option<Arc<dyn awaken_ext_memory::MemoryExtractionRepository>>,
}

/// Resources-context ports. Consumer: Session resource staging, Memory
/// recall/extraction, the Files/Artifacts adapters, and harvest.
pub(crate) struct ResourcePorts {
    /// Host-level memory auxiliary-agent capability. It owns no store: every
    /// recall/extraction operation requires a Session-scoped governed binding.
    pub(crate) memory: Arc<crate::memory::MemoryRuntime>,
    pub(crate) memory_terminal_observers: Arc<crate::memory::MemoryTerminalObservers>,
    /// Content-addressed blob store backing the Files API, file-resource mounts, and
    /// collected artifacts. A database-less Worker carries a fail-closed adapter;
    /// immutable claim-scoped reads use `file_content_source` instead.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) file_store: Arc<dyn FileStore>,
    /// Sole per-kind File materialization service. An embedded process points it at
    /// the local catalog/store pair; a database-less Worker replaces it with the
    /// claim-fenced HTTP adapter before accepting work.
    pub(crate) file_content_source:
        Arc<dyn crate::FileContentSource<crate::ResourceOperationFence>>,
    /// Logical Files-API truth: public identity, metadata, Workspace visibility,
    /// Session scope, and harvest idempotency. Bytes remain in `file_store` only.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) file_catalog: Arc<dyn FileCatalog>,
    /// Full Resources-owned File application retained for local management and
    /// test-support composition. Product Coordinator and database-less Worker
    /// processes receive only the narrower content and publication ports.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) file_application: Option<Arc<dyn awaken_resource_contract::FileApplicationService>>,
    /// Durable resource-plane lifecycle/reference state. It contains intrinsic
    /// Workspace/resource edges only and is independent of the IAM deployment.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) resource_reclamation:
        Option<Arc<dyn awaken_resource_contract::ResourceReclamationRepository>>,
    /// The Resources context's path-addressed Memory backend shared by API,
    /// mounts, recall, and extraction. See [`awaken_resource_contract::MemoryExecutionContent`].
    pub(crate) memory_stores: awaken_resource_contract::MemoryExecutionContent,
}

/// The Resources-context ports a composition selects before the Host
/// exists: [`ResourcePorts`] without the Host-owned Memory runtime, plus the
/// Memory extraction authority that runtime is built over. Consumer:
/// [`crate::SessionExecutorBuilder`].
pub(crate) struct ResourceSelection {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) file_store: Arc<dyn FileStore>,
    pub(crate) file_content_source:
        Arc<dyn crate::FileContentSource<crate::ResourceOperationFence>>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) file_catalog: Arc<dyn FileCatalog>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) file_application: Option<Arc<dyn awaken_resource_contract::FileApplicationService>>,
    pub(crate) artifact_publisher:
        Arc<dyn awaken_resource_contract::ArtifactPublisher<crate::ResourceOperationFence>>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) resource_reclamation:
        Option<Arc<dyn awaken_resource_contract::ResourceReclamationRepository>>,
    pub(crate) memory_stores: awaken_resource_contract::MemoryExecutionContent,
    /// The durable Memory extraction authority the Host's Memory runtime
    /// claims and records extraction intents through.
    pub(crate) extraction_repository: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
}

impl ResourcePorts {
    /// The selected ports around the Host's Memory runtime.
    pub(in crate::host) fn new(
        selection: ResourceSelection,
        memory: Arc<crate::memory::MemoryRuntime>,
        sessions: std::sync::Weak<crate::InstalledSessions>,
        bindings: awaken_session_run_delivery::MemoryBindingCompiler,
        local_workspace: String,
        direct_config: Option<serde_json::Value>,
    ) -> Self {
        Self {
            memory_terminal_observers: Arc::new(crate::memory::MemoryTerminalObservers::new(
                sessions,
                bindings,
                memory.clone(),
                local_workspace,
                direct_config,
            )),
            memory,
            #[cfg(any(test, feature = "test-support"))]
            file_store: selection.file_store,
            file_content_source: selection.file_content_source,
            #[cfg(any(test, feature = "test-support"))]
            file_catalog: selection.file_catalog,
            #[cfg(any(test, feature = "test-support"))]
            file_application: selection.file_application,
            #[cfg(any(test, feature = "test-support"))]
            resource_reclamation: selection.resource_reclamation,
            memory_stores: selection.memory_stores,
        }
    }
}

impl ResourceSelection {
    /// Select every Resource port once from the construction inputs. Product
    /// Resource content is injected atomically through `resources`; a Worker
    /// receives its remote adapters in `worker_content`. Only test-support
    /// construction may still use the local openers below.
    pub(in crate::host) fn assemble(
        inputs: ResourceInputs,
        store_dir: Option<&std::path::Path>,
        clock: &Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        let ResourceInputs {
            #[cfg(any(test, feature = "test-support"))]
                local: resources,
            worker_content,
            extraction_repository,
        } = inputs;
        #[cfg(not(test))]
        let _ = (store_dir, clock);
        #[cfg(any(test, feature = "test-support"))]
        let local_memory = resources.as_ref().map(|plane| plane.memory.clone());
        #[cfg(not(any(test, feature = "test-support")))]
        let local_memory: Option<awaken_resource_contract::MemoryExecutionContent> = None;
        let memory_stores = if let Some(content) = &worker_content {
            content.memory.clone()
        } else if let Some(content) = local_memory {
            content
        } else {
            #[cfg(test)]
            {
                crate::memory_stores::open(store_dir, clock.clone())
            }
            #[cfg(not(test))]
            unreachable!("Host construction requires explicit Resource authorities")
        };
        let extraction_repository = match extraction_repository {
            Some(repository) => repository,
            None if worker_content.is_some() => {
                Arc::new(crate::unavailable_worker::UnavailableWorkerExtractions)
                    as Arc<dyn awaken_ext_memory::MemoryExtractionRepository>
            }
            // Every construction, test fixtures included, injects its extraction
            // repository; the local test constructors open theirs before building.
            None => {
                unreachable!("Host construction requires an explicit Memory extraction repository")
            }
        };
        #[cfg(any(test, feature = "test-support"))]
        let (file_store, file_catalog) = if worker_content.is_some() {
            let files = Arc::new(crate::unavailable_worker::UnavailableWorkerFiles);
            (
                files.clone() as Arc<dyn awaken_resource_contract::FileStore>,
                files as Arc<dyn awaken_resource_contract::FileCatalog>,
            )
        } else {
            resources.as_ref().map_or_else(
                || match store_dir {
                    Some(_) => unreachable!(
                        "product Host construction requires an injected Resource component"
                    ),
                    None => {
                        #[cfg(test)]
                        {
                            let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
                            (
                                files.clone() as Arc<dyn awaken_resource_contract::FileStore>,
                                files as Arc<dyn awaken_resource_contract::FileCatalog>,
                            )
                        }
                        #[cfg(not(test))]
                        unreachable!("Host construction requires an explicit Resource component")
                    }
                },
                |plane| (plane.file_store.clone(), plane.file_catalog.clone()),
            )
        };
        #[cfg(any(test, feature = "test-support"))]
        let resource_reclamation = resources
            .as_ref()
            .and_then(|plane| plane.reclamation.clone());
        #[cfg(test)]
        let resource_reclamation =
            resource_reclamation.or_else(|| Some(crate::host::tests::test_resource_reclamation()));
        #[cfg(all(not(test), feature = "test-support"))]
        let file_application: Option<
            Arc<dyn awaken_resource_contract::FileApplicationService>,
        > = None;
        #[cfg(test)]
        let file_application = resource_reclamation.as_ref().map(|reclamation| {
            Arc::new(awaken_resource_application::FileApplication::new(
                file_store.clone(),
                file_catalog.clone(),
                reclamation.clone(),
                clock.clone(),
            )) as Arc<dyn awaken_resource_contract::FileApplicationService>
        });
        let file_content_source: Arc<dyn crate::FileContentSource<crate::ResourceOperationFence>> =
            worker_content
                .as_ref()
                .map(|content| content.file_content_source.clone())
                .unwrap_or_else(|| {
                    #[cfg(test)]
                    if let Some(application) = &file_application {
                        return Arc::new(
                            awaken_resource_application::ApplicationFileContentSource::new(
                                application.clone(),
                            ),
                        );
                    }
                    Arc::new(awaken_resource_contract::UnavailableFileContentSource)
                        as Arc<dyn crate::FileContentSource<crate::ResourceOperationFence>>
                });
        let artifact_publisher: Arc<
            dyn awaken_resource_contract::ArtifactPublisher<crate::ResourceOperationFence>,
        > = {
            #[cfg(test)]
            if let Some(application) = &file_application {
                Arc::new(
                    awaken_resource_application::ApplicationArtifactPublisher::new(
                        application.clone(),
                    ),
                )
            } else {
                Arc::new(awaken_resource_contract::UnavailableArtifactPublisher)
            }
            #[cfg(not(test))]
            Arc::new(awaken_resource_contract::UnavailableArtifactPublisher)
        };
        Self {
            #[cfg(any(test, feature = "test-support"))]
            file_store,
            file_content_source,
            #[cfg(any(test, feature = "test-support"))]
            file_catalog,
            #[cfg(any(test, feature = "test-support"))]
            file_application,
            artifact_publisher,
            #[cfg(any(test, feature = "test-support"))]
            resource_reclamation,
            memory_stores,
            extraction_repository,
        }
    }
}

impl ExecutorComponents {
    #[cfg(any(test, feature = "test-support"))]
    pub fn resource_reclamation(
        &self,
    ) -> Option<Arc<dyn awaken_resource_contract::ResourceReclamationRepository>> {
        self.resources.resource_reclamation.clone()
    }

    /// The content-addressed blob store (Files API, file-resource mounts, artifacts).
    #[cfg(any(test, feature = "test-support"))]
    pub fn file_store(&self) -> Arc<dyn FileStore> {
        self.resources.file_store.clone()
    }

    /// The sole durable logical-file catalog. Public Files identities resolve
    /// through this catalog before their private content-addressed blob is read.
    #[cfg(any(test, feature = "test-support"))]
    pub fn file_catalog(&self) -> Arc<dyn FileCatalog> {
        self.resources.file_catalog.clone()
    }

    /// The sole Resources-owned logical-File command application. Database-less
    /// Workers deliberately return `None`; they may only use `FileContentSource`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn file_application(
        &self,
    ) -> Option<Arc<dyn awaken_resource_contract::FileApplicationService>> {
        self.resources.file_application.clone()
    }
}

/// Return the exact controlled content held by this Host for fixture identity checks.
#[cfg(any(test, feature = "test-support"))]
pub fn test_memory_content(
    host: &ExecutorComponents,
) -> awaken_resource_contract::MemoryExecutionContent {
    host.resources.memory_stores.clone()
}
