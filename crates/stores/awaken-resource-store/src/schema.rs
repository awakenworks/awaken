//! Scoped schema for durable resource lifecycle state.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub const BUNDLE_ID: &str = "awaken.resource_lifecycle";
pub const NS: &str = "resource_lifecycle";
pub const CATALOG_BUNDLE_ID: &str = "awaken.resource_catalog";
pub const CATALOG_NS: &str = "resource_catalog";

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

pub fn resource_lifecycle_bundle() -> Result<MigrationBundle, MigrationError> {
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

/// Resources-owned catalog aggregate, independently versioned from lifecycle
/// fencing because the two aggregates have no table-level dependency.
pub fn resource_catalog_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        CATALOG_BUNDLE_ID,
        vec![Migration::new(
            1,
            "create resource catalog aggregate",
            include_str!("migrations/V0001__resource_catalog.sql").trim(),
        )?],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_lifecycle_bundle_lints() {
        // Cause/effect rule: both independently deployable Resources aggregates
        // must have unique version streams and may reference only their own
        // tables; lint success is the static ownership proof.
        let bundles = [
            resource_lifecycle_bundle().expect("lifecycle bundle builds"),
            resource_catalog_bundle().expect("catalog bundle builds"),
        ];
        awaken_scoped_migration::lint(&bundles).expect("resource bundles lint");
    }
}
