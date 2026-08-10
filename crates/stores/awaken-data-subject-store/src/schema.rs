//! Versioned schema for the Control-owned data-subject store.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub const CONTROL_BUNDLE_ID: &str = "awaken.control_data_subject";
pub const CONTROL_PREFIX: &str = "control_data_subject";

const CONTROL_FILES: &[(&str, &str)] = &[
    (
        "V0001__control_data_subject.sql",
        include_str!("migrations/V0001__control_data_subject.sql"),
    ),
    (
        "V0002__control_erasure_job.sql",
        include_str!("migrations/V0002__control_erasure_job.sql"),
    ),
    (
        "V0003__subject_revision.sql",
        include_str!("migrations/V0003__subject_revision.sql"),
    ),
    (
        "V0004__erasure_job_revision.sql",
        include_str!("migrations/V0004__erasure_job_revision.sql"),
    ),
];

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
        .find_map(|line| line.strip_prefix("--").map(|rest| rest.trim().to_owned()))
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| name.to_owned())
}

fn bundle(
    bundle_id: &'static str,
    files: &[(&str, &str)],
) -> Result<MigrationBundle, MigrationError> {
    let migrations = files
        .iter()
        .map(|(name, contents)| {
            let version = version_of(name);
            let description = description_of(name, contents);
            let sql = contents.trim();
            Migration::new(version, description, sql)
        })
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(bundle_id, migrations)
}

pub fn control_data_subject_bundle() -> Result<MigrationBundle, MigrationError> {
    bundle(CONTROL_BUNDLE_ID, CONTROL_FILES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_bundle_is_lint_clean() {
        // Cause/effect: the Control subject/erasure files produce one bundle and
        // one prefix; deterministic migration lint rejects unsafe DDL.
        let control = control_data_subject_bundle().expect("Control bundle builds");
        assert_eq!(control.bundle_id(), CONTROL_BUNDLE_ID);
        awaken_scoped_migration::lint(&[control]).expect("lint");
    }

    #[test]
    fn control_bundle_has_one_dense_unreleased_history() {
        /* Causes: C1 files are registered in filename order; C2 every body uses
         * the ordinary deterministic constructor. Effects: E1 expose one dense
         * V1..V4 stream; E2 reject a duplicate/renumbered version at bundle
         * construction. Decision rule D1=C1+C2=>E1. The migration mechanism's
         * bundle tests own E2; this test owns the store's exact current stream. */
        let bundle = control_data_subject_bundle().expect("bundle builds");
        assert_eq!(
            bundle
                .migrations()
                .iter()
                .map(Migration::version)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
    }
}
