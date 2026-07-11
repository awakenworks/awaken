//! The data-subject schema (ADR-0050). One portable [`MigrationBundle`] under
//! the `data_subject` namespace with its own ledger; the whole subject aggregate
//! (id, org, external_id, consents) serializes into the `data {json}` column.
//!
//! The DDL is NOT encoded here: every migration is a `.sql` file under
//! `migrations/`, embedded with `include_str!`. The file name carries the version
//! (`V0003__…` ⇒ version 3) and the first `-- comment` line is its description.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Namespaced bundle id — the split/merge unit for the data-subject domain.
pub const BUNDLE_ID: &str = "awaken.data_subject";

/// Embedded migration files, in apply order (`(name, contents)`): the name yields
/// the version, the contents the description (first `-- comment`) and SQL body.
const FILES: &[(&str, &str)] = &[
    (
        "V0001__subject.sql",
        include_str!("migrations/V0001__subject.sql"),
    ),
    (
        "V0002__captured.sql",
        include_str!("migrations/V0002__captured.sql"),
    ),
    (
        "V0003__restricted.sql",
        include_str!("migrations/V0003__restricted.sql"),
    ),
];

/// Version from a `Vnnnn__slug.sql` file name (`V0003__…` ⇒ 3); a non-positive
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

/// Build the data-subject migration bundle (prefix `data_subject`).
pub fn data_subject_bundle() -> Result<MigrationBundle, MigrationError> {
    let migrations = FILES
        .iter()
        .map(|(name, contents)| {
            Migration::new(version_of(name), description_of(name, contents), contents.trim())
        })
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(BUNDLE_ID, migrations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_subject_bundle_lints() {
        let bundle = data_subject_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }
}
