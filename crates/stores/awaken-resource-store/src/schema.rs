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

    #[test]
    fn resource_registry_bundle_preserves_published_v2_identity() {
        // Test design — cause/effect graph: C1 an installed database may carry
        // the published `awaken.resource_catalog` V1+V2 receipts; C2 the Rust
        // owner is now named Registry while the durable identity remains the
        // published Catalog-era value; C3 the runner validates version,
        // SQL-derived checksum, and description. Effects: E1 retain
        // the same bundle/table namespace and ordered versions; E2 retain V2's
        // exact SQL, checksum, and Registry-era description. Constraint: C2
        // cannot create another namespace or rewrite a receipt. Decision rule
        // R1: C1+C2+C3 => E1+E2; any drift must fail this static contract test.
        let bundle = resource_registry_bundle().expect("Registry bundle builds");
        assert_eq!(bundle.bundle_id(), "awaken.resource_catalog", "R1/E1");
        assert_eq!(REGISTRY_NS, "resource_catalog", "R1/E1");
        assert_eq!(
            bundle
                .migrations()
                .iter()
                .map(Migration::version)
                .collect::<Vec<_>>(),
            vec![1, 2],
            "R1/E1"
        );

        let v2 = &bundle.migrations()[1];
        assert_eq!(
            v2.description(),
            "add Resource Registry aggregate revision",
            "R1/E2"
        );
        assert_eq!(
            v2.sql_for(awaken_scoped_migration::Dialect::Sqlite),
            "-- add backend-neutral optimistic concurrency to Resource Registry aggregates\n\
             ALTER TABLE {prefix}_entry\n\
             ADD COLUMN revision BIGINT NOT NULL DEFAULT 1",
            "R1/E2"
        );
        assert_eq!(
            v2.checksum_for(awaken_scoped_migration::Dialect::Sqlite),
            "89410ffca9fa20adc47741fd36923b27dd3f1824ee1b35a58c91ad7c7bd903b6",
            "R1/E2"
        );
        assert_eq!(
            v2.checksum_for(awaken_scoped_migration::Dialect::Postgres),
            v2.checksum_for(awaken_scoped_migration::Dialect::Sqlite),
            "R1/E2 portable checksum"
        );
    }
}
