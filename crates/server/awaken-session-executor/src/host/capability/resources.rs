//! RES capability of [`SharedHost`]: the Resources-context ports the Host
//! consumes (Memory content and extraction, File content, Artifact
//! publication) and the test-support local File/Skill/reclamation authorities.

use crate::host::*;

/// Worker-side Resource content ports that must be installed as one unit.
/// Keeping the pair named prevents a partially remote Host startup from
/// silently opening a local content authority for the missing half.
pub(in crate::host) struct WorkerContentAdapters {
    pub(in crate::host) file_content_source:
        Arc<dyn crate::FileContentSource<awaken_dispatch_contract::ResourceOperationFence>>,
    pub(in crate::host) memory_repository: Arc<dyn awaken_resource_contract::MemoryRepository>,
}

/// Local Resource authorities a test or scenario composition hands to the
/// Host as one unit: the ports its Resources application exposes. Consumer:
/// `SharedHost::new_with_resources*`; `awaken-runtime-host-testkit` builds it
/// from a Resources application, which this crate no longer links.
#[cfg(any(test, feature = "test-support"))]
pub struct LocalResourcePorts {
    pub memory_repository: Arc<dyn awaken_resource_contract::MemoryRepository>,
    pub file_store: Arc<dyn awaken_resource_contract::FileStore>,
    pub file_catalog: Arc<dyn awaken_resource_contract::FileCatalog>,
    /// The durable delivered-Skill catalog, when the composition has one.
    pub skills: Option<LocalSkillPorts>,
    /// Resource lifecycle/reference state, when the composition has one.
    pub reclamation: Option<Arc<dyn awaken_resource_contract::ResourceReclamationRepository>>,
    /// The composition's clock, which also becomes the Host's process clock.
    pub clock: Arc<dyn awaken_time::WallClock>,
}

/// The durable Skill catalog and the two Resources applications over it that
/// the Host reads and freezes custom Skills through. Consumer:
/// [`LocalResourcePorts`].
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone)]
pub struct LocalSkillPorts {
    pub store: Arc<dyn awaken_resource_contract::SkillStore>,
    pub bundle_source: Arc<
        dyn awaken_session_configuration_contract::SkillBundleSource<
                awaken_dispatch_contract::ResourceOperationFence,
            >,
    >,
    pub application: Arc<dyn awaken_session_configuration_contract::SkillCatalogApplication>,
}

/// The Resource inputs of one Host construction, selected together so
/// [`ResourcePorts::assemble`] sees every source at once. Consumer:
/// `SharedHost::build`.
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
    /// Content-addressed blob store backing the Files API, file-resource mounts, and
    /// collected artifacts. A database-less Worker carries a fail-closed adapter;
    /// immutable claim-scoped reads use `file_content_source` instead.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) file_store: Arc<dyn FileStore>,
    /// Sole per-kind File materialization service. An embedded process points it at
    /// the local catalog/store pair; a database-less Worker replaces it with the
    /// claim-fenced HTTP adapter before accepting work.
    pub(crate) file_content_source:
        Arc<dyn crate::FileContentSource<awaken_dispatch_contract::ResourceOperationFence>>,
    /// Logical Files-API truth: public identity, metadata, Workspace visibility,
    /// Session scope, and harvest idempotency. Bytes remain in `file_store` only.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) file_catalog: Arc<dyn FileCatalog>,
    /// Full Resources-owned File application retained for local management and
    /// test-support composition. Product Coordinator and database-less Worker
    /// processes receive only the narrower content and publication ports.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) file_application: Option<Arc<dyn awaken_resource_contract::FileApplicationService>>,
    /// Sole Runtime-to-Resources artifact command edge. Embedded deployments
    /// install the local application adapter; database-less Workers install the
    /// claim-fenced HTTP client.
    pub(crate) artifact_publisher: Arc<
        dyn awaken_resource_contract::ArtifactPublisher<
                awaken_dispatch_contract::ArtifactPublicationFence,
            >,
    >,
    /// Durable resource-plane lifecycle/reference state. It contains intrinsic
    /// Workspace/resource edges only and is independent of the IAM deployment.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) resource_reclamation:
        Option<Arc<dyn awaken_resource_contract::ResourceReclamationRepository>>,
    /// The Resources context's path-addressed Memory backend shared by API,
    /// mounts, recall, and extraction. See [`crate::memory_stores`].
    pub(crate) memory_stores: crate::memory_stores::MemoryStores,
}

impl ResourcePorts {
    /// Select every Resource port once from the construction inputs. Product
    /// Resource content is injected atomically through `resources`; a Worker
    /// receives its remote adapters in `worker_content`. Only test-support
    /// construction may still use the local openers below.
    pub(in crate::host) fn assemble(
        llm: &Arc<dyn LlmExecutor>,
        inputs: ResourceInputs,
        store_dir: Option<&std::path::Path>,
        clock: &Arc<dyn awaken_time::WallClock>,
        memory_sandboxes: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxProvider>,
    ) -> Self {
        let ResourceInputs {
            #[cfg(any(test, feature = "test-support"))]
                local: resources,
            worker_content,
            extraction_repository,
        } = inputs;
        #[cfg(not(test))]
        let _ = store_dir;
        #[cfg(any(test, feature = "test-support"))]
        let local_memory = resources
            .as_ref()
            .map(|plane| plane.memory_repository.clone());
        #[cfg(not(any(test, feature = "test-support")))]
        let local_memory: Option<Arc<dyn awaken_resource_contract::MemoryRepository>> = None;
        let memory_stores = if let Some(content) = &worker_content {
            crate::memory_stores::MemoryStores::with_repository(content.memory_repository.clone())
        } else if let Some(repository) = local_memory {
            crate::memory_stores::MemoryStores::with_repository(repository)
        } else {
            // Only the Host's own unit tests construct without a Resource
            // component; every other composition injects one.
            #[cfg(test)]
            {
                crate::memory_stores::MemoryStores::open(store_dir, clock.clone())
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
            None => {
                #[cfg(test)]
                {
                    local_memory_extraction_repository(store_dir)
                }
                #[cfg(not(test))]
                unreachable!("Host construction requires an explicit Memory extraction repository")
            }
        };
        let memory = Arc::new(crate::memory::MemoryRuntime::new(
            llm.clone(),
            memory_sandboxes,
            Arc::new(BackgroundRuns::new()),
            extraction_repository,
            clock.clone(),
        ));
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
        let file_content_source: Arc<
            dyn crate::FileContentSource<awaken_dispatch_contract::ResourceOperationFence>,
        > = worker_content
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
                    as Arc<
                        dyn crate::FileContentSource<
                                awaken_dispatch_contract::ResourceOperationFence,
                            >,
                    >
            });
        let artifact_publisher: Arc<
            dyn awaken_resource_contract::ArtifactPublisher<
                    awaken_dispatch_contract::ArtifactPublicationFence,
                >,
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
            memory,
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
        }
    }
}

impl SharedHost {
    /// The local extraction repository the Host's own volatile/local unit
    /// fixtures use. Other compositions obtain theirs from
    /// `awaken-runtime-host-testkit` or inject a role-owned one.
    #[cfg(test)]
    pub fn test_memory_extraction_repository(
        storage_dir: Option<&std::path::Path>,
    ) -> Arc<dyn awaken_ext_memory::MemoryExtractionRepository> {
        local_memory_extraction_repository(storage_dir)
    }

    /// Install the one Resources-owned File command application. Runtime stores
    /// only this inward port and cannot construct a parallel implementation.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_file_application(
        mut self,
        application: Arc<dyn awaken_resource_contract::FileApplicationService>,
        content_source: Arc<
            dyn crate::FileContentSource<awaken_dispatch_contract::ResourceOperationFence>,
        >,
        artifact_publisher: Arc<
            dyn awaken_resource_contract::ArtifactPublisher<
                    awaken_dispatch_contract::ArtifactPublicationFence,
                >,
        >,
    ) -> Self {
        self.resources.file_content_source = content_source;
        self.resources.artifact_publisher = artifact_publisher;
        self.resources.file_application = Some(application);
        self
    }

    /// Install the narrow artifact publisher used by a database-less Worker.
    /// This does not grant File management or catalog access.
    #[must_use]
    pub fn with_artifact_publisher(
        mut self,
        publisher: Arc<
            dyn awaken_resource_contract::ArtifactPublisher<
                    awaken_dispatch_contract::ArtifactPublicationFence,
                >,
        >,
    ) -> Self {
        // This is the database-less Worker startup edge. Retaining the
        // test-only/local File application here would create a second command
        // path and incorrectly classify the remote publisher as locally fenced.
        #[cfg(any(test, feature = "test-support"))]
        {
            self.resources.file_application = None;
        }
        self.resources.artifact_publisher = publisher;
        self
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn resource_reclamation(
        &self,
    ) -> Option<Arc<dyn awaken_resource_contract::ResourceReclamationRepository>> {
        self.resources.resource_reclamation.clone()
    }

    /// Test-support replacement of a volatile fixture's File content port.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_file_content_source(
        mut self,
        source: Arc<
            dyn crate::FileContentSource<awaken_dispatch_contract::ResourceOperationFence>,
        >,
    ) -> Self {
        self.resources.file_content_source = source;
        self
    }
}

#[cfg(test)]
fn local_memory_extraction_repository(
    storage_dir: Option<&std::path::Path>,
) -> Arc<dyn awaken_ext_memory::MemoryExtractionRepository> {
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
