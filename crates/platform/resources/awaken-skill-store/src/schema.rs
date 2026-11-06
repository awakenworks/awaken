//! The skill-store schema. One portable [`MigrationBundle`] under the
//! `skill_store` namespace; its one table stores the complete versioned aggregate.
//! The same bundle renders on sqlite and postgres — the schema is written once.
//!
//! The DDL is a `.sql` file under `migrations/`, embedded with `include_str!`: the
//! file name carries the version (`V0001__…` ⇒ version 1) and the first
//! `-- comment` line is its description.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

mod expanded;

/// Namespaced bundle id — the split/merge unit for the skill-store domain.
pub const BUNDLE_ID: &str = "awaken.skill_store";
pub(crate) const CONVERGED_BUNDLE_ID: &str = "awaken.skill_store.converged";

/// Embedded migration files, in apply order (`(name, contents)`).
const FILES: &[(&str, &str)] = &[(
    "V0001__aggregate.sql",
    include_str!("migrations/V0001__aggregate.sql"),
)];

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

/// Build the skill-store migration bundle (prefix `skill_store`).
pub fn skill_store_bundle() -> Result<MigrationBundle, MigrationError> {
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

pub(crate) fn selected_skill_store_bundle(
    v1_checksum: Option<&str>,
) -> Result<MigrationBundle, MigrationError> {
    if v1_checksum == Some(expanded::V1_CHECKSUM) {
        expanded::bundle()
    } else {
        skill_store_bundle()
    }
}

#[cfg(all(test, feature = "sqlite"))]
pub(crate) fn expanded_skill_store_bundle() -> Result<MigrationBundle, MigrationError> {
    expanded::bundle()
}

pub(crate) fn converged_skill_store_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        CONVERGED_BUNDLE_ID,
        vec![Migration::new(
            1,
            "seal the converged skill-store migration history",
            "SELECT 1",
        )?],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_scoped_migration::{Dialect, MigrationError, plan};

    #[test]
    fn skill_store_bundle_lints() {
        // Decision table: S1 empty ledger -> one aggregate baseline; S2 exact V1
        // receipt -> no SQL; S3 drifted V1 -> fail closed. The common runner owns
        // S2/S3; this test owns the absence of the retired projection table.
        let bundle = skill_store_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, vec![1]);
        assert!(
            !bundle.migrations()[0]
                .sql_for(awaken_scoped_migration::Dialect::Sqlite)
                .contains("_skill ")
        );
    }

    #[test]
    fn published_histories_select_exact_v1_and_converge() {
        // Causes: H1 no/current V1, H2 exact expanded V1, H3 unknown V1.
        // Effects: E1 compact aggregate baseline, E2 exact projection+aggregate
        // history, E3 ordinary checksum rejection, E4 one common append stream.
        // Decision rules: H1=>E1+E4; H2=>E2+E4; H3=>E3.
        let compact = skill_store_bundle().expect("compact");
        let expanded = expanded::bundle().expect("expanded");
        let compact_v1 = compact.migrations()[0].checksum_for(Dialect::Sqlite);
        assert_eq!(selected_skill_store_bundle(None).expect("H1"), compact);
        assert_eq!(
            selected_skill_store_bundle(Some(&compact_v1)).expect("H1 current"),
            compact
        );
        assert_eq!(
            selected_skill_store_bundle(Some(expanded::V1_CHECKSUM)).expect("H2"),
            expanded
        );
        assert_eq!(
            expanded.migrations()[1].checksum_for(Dialect::Sqlite),
            expanded::V2_CHECKSUM,
            "H2 exact published V2"
        );
        awaken_scoped_migration::lint(std::slice::from_ref(
            &converged_skill_store_bundle().expect("converged"),
        ))
        .expect("convergence lints");
        let unknown = std::collections::BTreeMap::from([(1, "f".repeat(64))]);
        assert!(matches!(
            plan(
                &selected_skill_store_bundle(Some(&"f".repeat(64))).expect("H3 select"),
                &unknown,
                Dialect::Sqlite,
            ),
            Err(MigrationError::ChecksumMismatch { version: 1, .. })
        ));
    }
}
