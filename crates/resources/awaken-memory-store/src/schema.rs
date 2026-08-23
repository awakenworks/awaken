//! The memory-store schema. One portable [`MigrationBundle`] under the
//! `memory_store` namespace. The stream contains only the active path-addressed
//! aggregate and its version journal/counters.
//!
//! The DDL is a `.sql` file under `migrations/`, embedded with `include_str!`: the
//! file name carries the version (`V0001__…` ⇒ version 1) and the first
//! `-- comment` line is its description.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Namespaced bundle id — the split/merge unit for the memory-store domain.
pub const BUNDLE_ID: &str = "awaken.memory_store";

/// Embedded migration files, in apply order (`(name, contents)`).
const FILES: &[(&str, &str)] = &[
    (
        "V0001__memories.sql",
        include_str!("migrations/V0001__memories.sql"),
    ),
    (
        "V0002__versions_and_counters.sql",
        include_str!("migrations/V0002__versions_and_counters.sql"),
    ),
    (
        "V0003__version_actors.sql",
        include_str!("migrations/V0003__version_actors.sql"),
    ),
];

/// Version from a `Vnnnn__slug.sql` file name (`V0001__…` ⇒ 1); a non-positive
/// value is rejected by [`Migration::new`], so a mis-named file fails loudly.
fn version_of(name: &str) -> i64 {
    name.trim_start_matches('V')
        .split("__")
        .next()
        .and_then(|digits| digits.parse::<i64>().ok())
        .unwrap_or(0)
}

/// The first `-- comment` line of the file — the description lives with the DDL.
fn description_of(name: &str, contents: &str) -> String {
    contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("--").map(|rest| rest.trim().to_string()))
        .filter(|desc| !desc.is_empty())
        .unwrap_or_else(|| name.to_string())
}

/// Build the memory-store migration bundle (prefix `memory_store`).
pub fn memory_store_bundle() -> Result<MigrationBundle, MigrationError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_bundle_lints() {
        // Cause/effect decision table:
        // | published ledger | bundle history       | effect                  |
        // | empty            | current V1..V2       | active schema applies   |
        // | exact V1 applied | current V1..V2       | only V2 runs            |
        // | drifted V1       | current V1..V2       | fail before mutation    |
        // The common runner owns suffix/drift behavior; this assertion owns the
        // Resources Memory store's one dense current stream.
        let bundle = memory_store_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, vec![1, 2, 3]);
    }
}
