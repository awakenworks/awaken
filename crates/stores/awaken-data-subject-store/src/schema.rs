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
            Migration::new(
                version_of(name),
                description_of(name, contents),
                contents.trim(),
            )
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
}
