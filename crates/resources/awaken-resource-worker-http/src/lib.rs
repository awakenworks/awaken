//! Claim-fenced Resource data-plane HTTP used by registered Workers.
//!
//! This crate owns the Coordinator-side HTTP boundary and its paired Worker
//! clients. Resource identity and lifecycle remain in `awaken-resource-application`;
//! dispatch claim authority remains in `awaken-run-ingress`.

mod artifact_publication_http;
mod file_content_http;
mod memory_transport;
mod repository_binding_http;
mod skill_bundle_transport;

pub use artifact_publication_http::{
    ARTIFACT_METADATA_HEADER, ARTIFACT_PUBLICATION_PATH, ArtifactPublicationRequest,
    HttpArtifactPublisher, WorkerArtifactPublicationService, worker_artifact_publication_router,
};
pub use file_content_http::{
    HttpFileContentSource, WorkerFileContentService, worker_file_content_router,
};
pub use memory_transport::{
    HttpMemoryMaterializationReferenceEncoder, HttpMemoryRepository, HttpMemorySnapshotSource,
    HttpMemoryWritebackClient, WorkerMemoryService, memory_materialization_reference,
    worker_memory_router,
};
pub use repository_binding_http::{
    HttpRepositoryBindingVerifier, RepositoryTransportAuthority, RepositoryTransportAuthorization,
    RepositoryTransportAuthorizer, WorkerRepositoryBindingService,
    worker_repository_binding_router,
};
pub use skill_bundle_transport::{
    HttpSkillBundleSource, WorkerSkillBundleService, worker_skill_bundle_router,
};
