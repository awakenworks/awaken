//! Scoped schema for durable resource lifecycle state.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

// These identifiers are durable, published migration identities. Keep them
// stable even though the Rust-facing responsibility is named "reclamation".
pub const BUNDLE_ID: &str = "awaken.resource_lifecycle";
pub const NS: &str = "resource_lifecycle";
// The public responsibility is now named Registry, but these values identify
// an already-published migration stream and its tables. Renaming either would
// silently create a second empty store instead of upgrading existing data.
pub const REGISTRY_BUNDLE_ID: &str = "awaken.resource_catalog";
pub const REGISTRY_NS: &str = "resource_catalog";

const FILES: &[(&str, &str)] = &[(
    "V0001__resource_lifecycle.sql",
    include_str!("migrations/V0001__resource_lifecycle.sql"),
)];

fn version_of(name: &str) -> i64 {
    name.trim_start_matches('V')
        .split("__")
        .next()
        .and_then(|digits| digits.parse::<i64>().ok())
        .unwrap_or(0)
}

fn description_of(name: &str, contents: &str) -> String {
    contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("--").map(|rest| rest.trim().to_string()))
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| name.to_string())
}

pub fn resource_reclamation_bundle() -> Result<MigrationBundle, MigrationError> {
    let migrations = FILES
        .iter()
        .map(|(name, contents)| {
            Migration::new(
                version_of(name),
                description_of(name, contents),
                contents.trim(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(BUNDLE_ID, migrations)
}

/// Resources-owned Registry aggregate, independently versioned from lifecycle
/// fencing because the two aggregates have no table-level dependency.
pub fn resource_registry_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        REGISTRY_BUNDLE_ID,
        vec![
            Migration::new(
                1,
                // V1 metadata is part of the published migration identity.
                "create resource catalog aggregate",
                include_str!("migrations/V0001__resource_catalog.sql").trim(),
            )?,
            Migration::new(
                2,
                "add Resource Registry aggregate revision",
                include_str!("migrations/V0002__resource_registry_revision.sql").trim(),
            )?,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_reclamation_bundle_lints() {
        // Cause/effect rule: both independently deployable Resources aggregates
        // must have unique version streams and may reference only their own
        // tables; lint success is the static ownership proof.
        let bundles = [
            resource_reclamation_bundle().expect("lifecycle bundle builds"),
            resource_registry_bundle().expect("Registry bundle builds"),
        ];
        awaken_scoped_migration::lint(&bundles).expect("resource bundles lint");
    }
}
