// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: The registered Environment and authored base reference select a frozen recipe.
// @formal reason: This is a product-boundary classification, not a formal-proof claim for image identity.
//! Image recipe identity and the immutable registration-to-recipe coordinate.

use crate::ExecutableEnvironmentRegistration;
use awaken_environment_contract::{EnvironmentConfig, EnvironmentRevision};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentImageBuildDemand {
    pub build_key: String,
    pub environment_id: String,
    pub source_revision: EnvironmentRevision,
    pub definition_fingerprint: String,
    pub base_image: String,
    pub config: EnvironmentConfig,
}

impl EnvironmentImageBuildDemand {
    /// A frozen registration and its selected authored base reference pin one
    /// resolved recipe. Several pins may share the same content-addressed job.
    #[must_use]
    pub fn binding_key(
        registration: &ExecutableEnvironmentRegistration,
        base_reference: &str,
    ) -> String {
        format!(
            "environment-image-binding:{}",
            awaken_environment_contract::environment_facts_fingerprint(&(
                &registration.fingerprint,
                base_reference
            ),)
        )
    }

    #[must_use]
    pub fn from_registration(
        registration: &ExecutableEnvironmentRegistration,
        base_image: &str,
    ) -> Option<Self> {
        if registration.definition.config.is_self_hosted()
            || registration.definition.config.packages().is_empty()
        {
            return None;
        }
        // Only build inputs own image identity. Environment name, description,
        // metadata and authored revision remain provenance and must not create a
        // parallel image for identical package/network/base facts.
        let recipe_fingerprint = awaken_environment_contract::environment_facts_fingerprint(&(
            base_image,
            &registration.definition.config,
        ));
        let build_key = format!("environment-image:{recipe_fingerprint}");
        Some(Self {
            build_key,
            environment_id: registration.definition.id.clone(),
            source_revision: registration.definition.revision,
            definition_fingerprint: registration.fingerprint.clone(),
            base_image: base_image.to_owned(),
            config: registration.definition.config.clone(),
        })
    }

    /// Whether two demands name the same authoritative build recipe. Provenance
    /// may advance while a content-identical ready image remains reusable.
    #[must_use]
    pub fn same_recipe(&self, other: &Self) -> bool {
        self.build_key == other.build_key
            && self.base_image == other.base_image
            && self.config == other.config
    }
}
