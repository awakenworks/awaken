//! Resource transport contracts with independent per-test fixtures.
//!
//! Preserve the domain modules and every assertion while sharing one native
//! test executable. Memory retains the portability gate's dedicated target.

#[path = "artifact_publication_transport.rs"]
mod artifact_publication_transport;
#[path = "file_content_transport.rs"]
mod file_content_transport;
#[path = "repository_transport.rs"]
mod repository_transport;
#[path = "skill_bundle_transport.rs"]
mod skill_bundle_transport;
