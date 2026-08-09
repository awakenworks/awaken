//! The memory-store schema. One portable [`MigrationBundle`] under the
//! `memory_store` namespace. V0002 and V0003 define the active path-addressed
//! aggregate. V0001 is immutable migration history: its retired blob table has
//! no production adapter or port, but the migration must remain byte-identical
//! so databases created by earlier releases continue to verify and upgrade.
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
        "V0001__blob.sql",
        include_str!("migrations/V0001__blob.sql"),
    ),
    (
        "V0002__memories.sql",
        include_str!("migrations/V0002__memories.sql"),
    ),
    (
        "V0003__versions_and_counters.sql",
        include_str!("migrations/V0003__versions_and_counters.sql"),
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
        // | empty            | immutable V1..V3     | fresh schema applies    |
        // | V1/V2 applied    | same immutable V1..V3| only missing versions run|
        // | V1/V2 applied    | rewritten V1 only    | fail: unknown version   |
        // This structural assertion owns rules 1/2; migration-runner tests own
        // checksum and forward-application behavior. Keeping the retired V1 DDL
        // is migration compatibility, not a second active MemoryStore adapter.
        let bundle = memory_store_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, vec![1, 2, 3]);
    }
}
