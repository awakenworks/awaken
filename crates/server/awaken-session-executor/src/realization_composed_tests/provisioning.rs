//! Original Host composition tests over canonical realization code.
use crate::staged_resources::{RepositoryActivation, StagedResources};
use awaken_provisioning_contract as pc;
use awaken_resource_contract::ArtifactPublicationError;
use awaken_resource_contract::{FileCatalog, FileCatalogError, FileRecord};

use std::sync::Arc;

use awaken_session_configuration_contract::environment_projection::*;
use awaken_session_realization::provisioning::*;

#[cfg(test)]
#[path = "../provisioning/tests.rs"]
mod provisioning_registry_tests;
